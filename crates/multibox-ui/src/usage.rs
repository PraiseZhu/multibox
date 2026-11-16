//! How much disk a worktree takes, and what that space is made of.
//!
//! Every file is put in one of four groups: tracked by git, not tracked (the
//! engine caches Multibox carries and whatever else is ignored), hard-linked
//! (files sharing their data with another path, which is how Multibox shares
//! write-once caches on NTFS) and, for the main worktree, the git database.
//!
//! Hard-linked files are also reported by identity, so the window can count
//! data shared between worktrees once. Reflinked data (Dev Drive, APFS) cannot
//! be told apart from a copy this way; there the sizes are logical sizes.

use multibox_core::git::Repo;
use multibox_core::index::Index;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const TRASH_PREFIX: &str = ".multibox-trash-";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Split {
    pub tracked: u64,
    pub untracked: u64,
    pub linked: u64,
    pub git: u64,
}

impl Split {
    pub fn total(&self) -> u64 {
        self.tracked + self.untracked + self.linked + self.git
    }

    fn add(&mut self, kind: Kind, bytes: u64) {
        match kind {
            Kind::Tracked => self.tracked += bytes,
            Kind::Untracked => self.untracked += bytes,
            Kind::Linked => self.linked += bytes,
            Kind::Git => self.git += bytes,
        }
    }

    fn merge(&mut self, other: &Split) {
        self.tracked += other.tracked;
        self.untracked += other.untracked;
        self.linked += other.linked;
        self.git += other.git;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Tracked,
    Untracked,
    Linked,
    Git,
}

/// A file whose data has more than one name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinkedFile {
    pub volume: u64,
    pub id: u64,
    pub size: u64,
}

/// One entry at the top of the worktree.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub split: Split,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub files: u64,
    pub split: Split,
    /// Largest first.
    pub entries: Vec<Entry>,
    pub linked: Vec<LinkedFile>,
    /// Entries that could not be read.
    pub skipped: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

pub struct ScanRequest<'a> {
    pub root: &'a Path,
    /// Directories to leave out, relative to `root` with `/`: worktrees nested
    /// inside this one are measured on their own.
    pub skip: HashSet<String>,
    /// Paths git tracks, relative with `/`. Without them nothing is tracked.
    pub tracked: Option<HashSet<String>>,
    /// Look up hard links. Costs a file open per file on Windows.
    pub links: bool,
    pub progress: &'a ScanProgress,
}

/// The paths git tracks in the worktree at `root`, from its index.
pub fn tracked_paths(root: &Path) -> Option<HashSet<String>> {
    let repo = Repo::discover(root).ok()?;
    let index = Index::read(&repo.index_path(), repo.hash_len).ok()?;
    Some(index.entries.into_iter().map(|entry| entry.path).collect())
}

struct Queue {
    state: Mutex<(Vec<String>, usize)>,
    wake: Condvar,
}

impl Queue {
    fn next(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(dir) = state.0.pop() {
                state.1 += 1;
                return Some(dir);
            }
            if state.1 == 0 {
                self.wake.notify_all();
                return None;
            }
            state = self.wake.wait(state).unwrap();
        }
    }

    fn finish(&self, more: Vec<String>) {
        let mut state = self.state.lock().unwrap();
        state.0.extend(more);
        state.1 -= 1;
        self.wake.notify_all();
    }
}

#[derive(Default)]
struct Found {
    files: u64,
    skipped: u64,
    top: HashMap<String, (bool, Split)>,
    linked: Vec<LinkedFile>,
}

struct LinkInfo {
    links: u64,
    volume: u64,
    id: u64,
    size: u64,
}

#[cfg(windows)]
fn link_info(path: &Path, _meta: &fs::Metadata) -> Option<LinkInfo> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };
    // No access rights: reading attributes needs none, and neither an editor
    // holding the file nor a virus scanner is disturbed.
    let file = fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return None;
    }
    Some(LinkInfo {
        links: u64::from(info.nNumberOfLinks),
        volume: u64::from(info.dwVolumeSerialNumber),
        id: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        // The directory entry of a hard link can carry a stale size; the file's
        // own record cannot.
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
    })
}

#[cfg(unix)]
fn link_info(_path: &Path, meta: &fs::Metadata) -> Option<LinkInfo> {
    use std::os::unix::fs::MetadataExt;
    Some(LinkInfo {
        links: meta.nlink(),
        volume: meta.dev(),
        id: meta.ino(),
        size: meta.len(),
    })
}

fn join(root: &Path, rel: &str) -> PathBuf {
    if cfg!(windows) {
        root.join(rel.replace('/', "\\"))
    } else {
        root.join(rel)
    }
}

fn walk_dir(root: &Path, rel: &str, request: &ScanRequest<'_>, found: &mut Found) -> Vec<String> {
    let mut more = Vec::new();
    let entries = match fs::read_dir(join(root, rel)) {
        Ok(entries) => entries,
        Err(_) => {
            found.skipped += 1;
            return more;
        }
    };
    let (mut files, mut bytes) = (0u64, 0u64);
    for entry in entries {
        let Ok(entry) = entry else {
            found.skipped += 1;
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(meta) = entry.metadata() else {
            found.skipped += 1;
            continue;
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            // Links are never followed: a junction could lead anywhere.
            continue;
        }
        if kind.is_dir() {
            if name.starts_with(TRASH_PREFIX) || request.skip.contains(&child) {
                continue;
            }
            if rel.is_empty() {
                found
                    .top
                    .entry(child.clone())
                    .or_insert((true, Split::default()));
            }
            more.push(child);
            continue;
        }

        let mut size = meta.len();
        let (top, rest) = child.split_once('/').unwrap_or((child.as_str(), ""));
        let in_git = top == ".git";
        let mut file_kind = None;
        if in_git {
            file_kind = Some(Kind::Git);
        } else if request.links
            && size > 0
            && let Some(info) = link_info(&entry.path(), &meta)
        {
            size = info.size;
            if info.links > 1 {
                found.linked.push(LinkedFile {
                    volume: info.volume,
                    id: info.id,
                    size,
                });
                file_kind = Some(Kind::Linked);
            }
        }
        let file_kind = file_kind.unwrap_or_else(|| match &request.tracked {
            Some(tracked) if tracked.contains(&child) => Kind::Tracked,
            _ => Kind::Untracked,
        });

        found
            .top
            .entry(top.to_owned())
            .or_insert((!rest.is_empty(), Split::default()))
            .1
            .add(file_kind, size);
        files += 1;
        bytes += size;
    }
    found.files += files;
    request.progress.files.fetch_add(files, Ordering::Relaxed);
    request.progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    more
}

/// Measures everything below `request.root`, in parallel.
pub fn scan(request: &ScanRequest<'_>) -> std::io::Result<Usage> {
    let started = Instant::now();
    let root = fs::canonicalize(request.root)?;
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(2, 16);
    let queue = Queue {
        state: Mutex::new((vec![String::new()], 0)),
        wake: Condvar::new(),
    };
    let results: Vec<Found> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut found = Found::default();
                    while let Some(rel) = queue.next() {
                        let more = if request.progress.cancel.load(Ordering::Relaxed) {
                            Vec::new()
                        } else {
                            walk_dir(&root, &rel, request, &mut found)
                        };
                        queue.finish(more);
                    }
                    found
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("scan worker panicked"))
            .collect()
    });

    let mut usage = Usage::default();
    let mut top: HashMap<String, (bool, Split)> = HashMap::new();
    for found in results {
        usage.files += found.files;
        usage.skipped += found.skipped;
        usage.linked.extend(found.linked);
        for (name, (dir, split)) in found.top {
            let slot = top.entry(name).or_insert((dir, Split::default()));
            slot.0 |= dir;
            slot.1.merge(&split);
        }
    }
    usage.entries = top
        .into_iter()
        .map(|(name, (dir, split))| {
            usage.split.merge(&split);
            Entry { name, dir, split }
        })
        .collect();
    usage.entries.sort_unstable_by(|a, b| {
        b.split
            .total()
            .cmp(&a.split.total())
            .then_with(|| a.name.cmp(&b.name))
    });
    usage.elapsed = started.elapsed();
    Ok(usage)
}

/// Total size of a directory, without classifying anything.
pub fn tree_size(path: &Path) -> u64 {
    let progress = ScanProgress::default();
    let request = ScanRequest {
        root: path,
        skip: HashSet::new(),
        tracked: None,
        links: false,
        progress: &progress,
    };
    scan(&request).map_or(0, |usage| usage.split.total())
}

/// Bytes the given worktrees take once data shared through hard links is
/// counted once, and the bytes that sharing saves.
pub fn deduplicated<'a>(reports: impl IntoIterator<Item = &'a Usage>) -> (u64, u64) {
    let mut logical = 0u64;
    let mut linked_logical = 0u64;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut linked_unique = 0u64;
    for usage in reports {
        logical += usage.split.total();
        for file in &usage.linked {
            linked_logical += file.size;
            if seen.insert((file.volume, file.id)) {
                linked_unique += file.size;
            }
        }
    }
    let saved = linked_logical - linked_unique;
    (logical - saved, saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn groups_by_tracking_and_shares_hard_links_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        write(&a.join("src/main.rs"), 100);
        write(&a.join("Library/cache.bin"), 1000);
        write(&a.join("Library/Artifacts/00/shared"), 5000);
        write(&a.join("nested/inner.txt"), 7);
        write(&a.join(".multibox-trash-x-1/junk"), 9999);
        write(&b.join("readme.md"), 10);
        fs::create_dir_all(b.join("Library/Artifacts/00")).unwrap();
        fs::hard_link(
            a.join("Library/Artifacts/00/shared"),
            b.join("Library/Artifacts/00/shared"),
        )
        .unwrap();

        let progress = ScanProgress::default();
        let usage_a = scan(&ScanRequest {
            root: &a,
            skip: HashSet::from(["nested".to_owned()]),
            tracked: Some(HashSet::from(["src/main.rs".to_owned()])),
            links: true,
            progress: &progress,
        })
        .unwrap();
        assert_eq!(usage_a.files, 3);
        assert_eq!(usage_a.split.tracked, 100);
        assert_eq!(usage_a.split.untracked, 1000);
        assert_eq!(usage_a.split.linked, 5000);
        assert_eq!(usage_a.entries[0].name, "Library");
        assert_eq!(usage_a.entries[0].split.linked, 5000);

        let usage_b = scan(&ScanRequest {
            root: &b,
            skip: HashSet::new(),
            tracked: None,
            links: true,
            progress: &progress,
        })
        .unwrap();
        assert_eq!(usage_b.split.linked, 5000);
        let (actual, saved) = deduplicated([&usage_a, &usage_b]);
        assert_eq!(saved, 5000);
        assert_eq!(actual, 100 + 1000 + 5000 + 10);
    }
}
