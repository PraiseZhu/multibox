//! How much disk a worktree takes, and what that space is made of.
//!
//! Every file is put in one of four groups: tracked by git, not tracked (the
//! engine caches Multibox carries and whatever else is ignored), hard-linked
//! (names whose data is counted elsewhere: shared with another path outside
//! the worktree, which is how Multibox shares write-once caches on NTFS, or a
//! second name of data the worktree already holds, as Cargo makes in
//! `target/`) and, for the main worktree, the git database. Data whose every
//! name is inside the worktree is the worktree's own: one name carries it as
//! tracked or not, the others are hard-linked.
//!
//! Hard-linked files are also reported by identity, so the window can count
//! data shared between worktrees once. Reflinked data (Dev Drive, APFS) cannot
//! be told apart from a copy this way; there the sizes are logical sizes.

use multibox_core::git::Repo;
use multibox_core::index::Index;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// Every name of hard-linked data found, own or shared.
    pub linked: Vec<LinkedFile>,
    /// Data shared with paths outside the worktree, each piece once: what the
    /// worktree would take more if it held a copy of its own.
    pub shared: u64,
    /// Entries that could not be read.
    pub skipped: u64,
    pub elapsed: Duration,
    /// What the next scan of this tree may take from this one.
    pub memo: Memo,
}

/// What a scan learned about each directory, for the next scan of the same
/// tree. A directory's own time moves whenever an entry in it is added,
/// removed or renamed, so while it stands still its files are the files they
/// were, with the links they had: only those found hard-linked need keeping,
/// and the next scan opens none of them. That opening is nearly all the time a
/// scan takes.
///
/// A link made or removed from outside the directory does not move its time.
/// Multibox makes and removes such links only with worktrees, so a memo is
/// kept under the list of worktrees it was made with and is dropped when the
/// list changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Memo {
    dirs: HashMap<String, MemoDir>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MemoDir {
    /// The directory's own last-write time in nanoseconds since 1970, or 0
    /// when it was too close to the scan to be sure nothing came after.
    written: u64,
    /// Its hard-linked files by name. Every other file in it had one name.
    linked: HashMap<String, LinkInfo>,
}

/// A directory time within this long of the scan is not relied on: a coarse
/// clock, or a write landing in the same tick, could hide a change.
const SETTLED: Duration = Duration::from_secs(2);

/// The start of a memo file, with the version of its layout.
const MEMO_MAGIC: &[u8; 8] = b"MBMEMO01";

impl Memo {
    /// Writes the memo to `path`, under `key`: what it was made with, such as
    /// the worktrees there were. Little-endian numbers, strings as their
    /// length and UTF-8.
    pub fn save(&self, path: &Path, key: &str) -> std::io::Result<()> {
        fn number(out: &mut Vec<u8>, value: u64) {
            out.extend_from_slice(&value.to_le_bytes());
        }
        fn text(out: &mut Vec<u8>, value: &str) {
            number(out, value.len() as u64);
            out.extend_from_slice(value.as_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(MEMO_MAGIC);
        text(&mut out, key);
        number(&mut out, self.dirs.len() as u64);
        for (rel, dir) in &self.dirs {
            text(&mut out, rel);
            number(&mut out, dir.written);
            number(&mut out, dir.linked.len() as u64);
            for (name, info) in &dir.linked {
                text(&mut out, name);
                for value in [info.links, info.volume, info.id, info.size] {
                    number(&mut out, value);
                }
            }
        }
        let parent = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut pending, &out)?;
        pending.persist(path)?;
        Ok(())
    }

    /// The memo at `path`, if there is one, it reads whole, and it was saved
    /// under `key`.
    pub fn load(path: &Path, key: &str) -> Option<Memo> {
        struct Reader<'a>(&'a [u8]);
        impl Reader<'_> {
            fn bytes(&mut self, count: usize) -> Option<&[u8]> {
                if count > self.0.len() {
                    return None;
                }
                let (head, rest) = self.0.split_at(count);
                self.0 = rest;
                Some(head)
            }
            fn number(&mut self) -> Option<u64> {
                Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
            }
            fn text(&mut self) -> Option<String> {
                let length = usize::try_from(self.number()?).ok()?;
                String::from_utf8(self.bytes(length)?.to_vec()).ok()
            }
        }
        let data = fs::read(path).ok()?;
        let mut reader = Reader(&data);
        if reader.bytes(MEMO_MAGIC.len())? != MEMO_MAGIC || reader.text()? != key {
            return None;
        }
        // Counts come from the file, so nothing is reserved ahead of them: a
        // damaged count runs out of data instead of memory.
        let dirs = reader.number()?;
        let mut memo = Memo::default();
        for _ in 0..dirs {
            let rel = reader.text()?;
            let written = reader.number()?;
            let mut linked = HashMap::new();
            for _ in 0..reader.number()? {
                let name = reader.text()?;
                let info = LinkInfo {
                    links: reader.number()?,
                    volume: reader.number()?,
                    id: reader.number()?,
                    size: reader.number()?,
                };
                linked.insert(name, info);
            }
            memo.dirs.insert(rel, MemoDir { written, linked });
        }
        reader.0.is_empty().then_some(memo)
    }
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
    /// The last scan of this tree, to take unchanged directories from.
    pub previous: Option<&'a Memo>,
    pub progress: &'a ScanProgress,
}

/// The paths git tracks in the worktree at `root`, from its index.
pub fn tracked_paths(root: &Path) -> Option<HashSet<String>> {
    let repo = Repo::discover(root).ok()?;
    let index = Index::read(&repo.index_path(), repo.hash_len).ok()?;
    Some(index.entries.into_iter().map(|entry| entry.path).collect())
}

/// The directories left to read, for every tree being measured.
struct Queue {
    state: Mutex<Waiting>,
    wake: Condvar,
}

struct Waiting {
    /// A stack of directories per tree, so each is read depth first.
    dirs: Vec<Vec<String>>,
    /// Directories being read.
    busy: usize,
    /// The tree the next directory comes from: they take turns, so a large
    /// tree does not hold up a small one queued beside it.
    turn: usize,
}

impl Queue {
    fn next(&self) -> Option<(usize, String)> {
        let mut state = self.state.lock().unwrap();
        loop {
            let trees = state.dirs.len();
            for step in 0..trees {
                let tree = (state.turn + step) % trees;
                if let Some(dir) = state.dirs[tree].pop() {
                    state.turn = tree + 1;
                    state.busy += 1;
                    return Some((tree, dir));
                }
            }
            if state.busy == 0 {
                self.wake.notify_all();
                return None;
            }
            state = self.wake.wait(state).unwrap();
        }
    }

    fn finish(&self, tree: usize, more: Vec<String>) {
        let mut state = self.state.lock().unwrap();
        state.dirs[tree].extend(more);
        state.busy -= 1;
        self.wake.notify_all();
    }
}

#[derive(Default)]
struct Found {
    files: u64,
    skipped: u64,
    top: HashMap<String, (bool, Split)>,
    names: Vec<Name>,
    memo: Vec<(String, MemoDir)>,
}

impl Found {
    fn absorb(&mut self, other: Found) {
        self.files += other.files;
        self.skipped += other.skipped;
        self.names.extend(other.names);
        self.memo.extend(other.memo);
        for (name, (dir, split)) in other.top {
            let slot = self.top.entry(name).or_insert((dir, Split::default()));
            slot.0 |= dir;
            slot.1.merge(&split);
        }
    }
}

/// A tree being measured beside others.
struct Tree<'r, 'a> {
    request: &'r ScanRequest<'a>,
    root: PathBuf,
    /// Directories queued or being read: the tree is done when none are.
    pending: AtomicUsize,
    found: Mutex<Found>,
}

/// One name of hard-linked data, as the walk found it.
struct Name {
    file: LinkedFile,
    /// How many names the data has, here and elsewhere.
    links: u64,
    /// The entry at the top of the worktree it is under.
    top: String,
    /// What it would count as, were it not linked.
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinkInfo {
    links: u64,
    volume: u64,
    id: u64,
    size: u64,
}

/// A directory's own last-write time, in nanoseconds since 1970.
fn written(dir: &Path) -> Option<u64> {
    let time = fs::metadata(dir).ok()?.modified().ok()?;
    let since = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
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
    let dir = join(root, rel);
    // The directory's own time, read before its entries: a change made while
    // they are read then shows next time rather than never. Git's database
    // is not looked into for links, so there is nothing to keep for it.
    let in_git = rel == ".git" || rel.starts_with(".git/");
    let remember = request.links && !in_git;
    let time = remember.then(|| written(&dir)).flatten();
    let unchanged = time.and_then(|time| {
        let previous = request.previous?.dirs.get(rel)?;
        (previous.written == time).then_some(previous)
    });
    let settled = time.filter(|&time| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |now| now.as_nanos());
        u128::from(time) + SETTLED.as_nanos() < now
    });
    let mut memo = MemoDir {
        written: settled.unwrap_or(0),
        linked: HashMap::new(),
    };

    let entries = match fs::read_dir(&dir) {
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
        let unlinked = || match &request.tracked {
            Some(tracked) if tracked.contains(&child) => Kind::Tracked,
            _ => Kind::Untracked,
        };
        // In an unchanged directory a file is linked as it was last time, or
        // not at all, and nothing needs opening.
        let looked_up = || match unchanged {
            Some(previous) => previous.linked.get(&name).copied(),
            None => link_info(&entry.path(), &meta),
        };
        let mut file_kind = None;
        if in_git {
            file_kind = Some(Kind::Git);
        } else if request.links
            && size > 0
            && let Some(info) = looked_up()
        {
            size = info.size;
            if info.links > 1 {
                memo.linked.insert(name.clone(), info);
                found.names.push(Name {
                    file: LinkedFile {
                        volume: info.volume,
                        id: info.id,
                        size,
                    },
                    links: info.links,
                    top: top.to_owned(),
                    kind: unlinked(),
                });
                file_kind = Some(Kind::Linked);
            }
        }
        let file_kind = file_kind.unwrap_or_else(unlinked);

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
    if remember {
        found.memo.push((rel.to_owned(), memo));
    }
    more
}

/// Measures everything below `request.root`, in parallel.
pub fn scan(request: &ScanRequest<'_>) -> std::io::Result<Usage> {
    let result = Mutex::new(None);
    scan_all(std::slice::from_ref(request), |_, usage| {
        *result.lock().unwrap() = Some(usage);
    });
    result
        .into_inner()
        .unwrap()
        .expect("scan_all reports every tree")
}

/// Measures several trees at once with one set of threads between them: all
/// of them move together, and a small one is done long before a large one.
/// `done` hears of each tree, by its place in `requests`, as soon as its last
/// directory has been read.
///
/// Nearly all the time goes to opening each file for its link count: 282,000
/// files of a worktree on a Dev Drive took 4 to 5 s that way, 0.15 s without.
/// The file system serves those opens largely one after another, so 32
/// threads were no faster than 16 on a 32-thread machine and 48 were slower;
/// trees read together take about as long as one after the other.
pub fn scan_all(requests: &[ScanRequest<'_>], done: impl Fn(usize, std::io::Result<Usage>) + Sync) {
    let started = Instant::now();
    let mut trees = Vec::with_capacity(requests.len());
    let mut dirs = Vec::with_capacity(requests.len());
    for (index, request) in requests.iter().enumerate() {
        match fs::canonicalize(request.root) {
            Ok(root) => {
                trees.push(Some(Tree {
                    request,
                    root,
                    pending: AtomicUsize::new(1),
                    found: Mutex::default(),
                }));
                dirs.push(vec![String::new()]);
            }
            Err(err) => {
                done(index, Err(err));
                trees.push(None);
                dirs.push(Vec::new());
            }
        }
    }
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(2, 16);
    let queue = Queue {
        state: Mutex::new(Waiting {
            dirs,
            busy: 0,
            turn: 0,
        }),
        wake: Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some((index, rel)) = queue.next() {
                    let tree = trees[index].as_ref().expect("only opened trees are queued");
                    let mut found = Found::default();
                    let more = if tree.request.progress.cancel.load(Ordering::Relaxed) {
                        Vec::new()
                    } else {
                        walk_dir(&tree.root, &rel, tree.request, &mut found)
                    };
                    // Count the directories found before this one is done, so
                    // the tree cannot look finished while they wait.
                    tree.pending.fetch_add(more.len(), Ordering::AcqRel);
                    tree.found.lock().unwrap().absorb(found);
                    queue.finish(index, more);
                    if tree.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                        let found = std::mem::take(&mut *tree.found.lock().unwrap());
                        done(index, Ok(usage_of(found, started.elapsed())));
                    }
                }
            });
        }
    });
}

/// What one tree's directories add up to.
fn usage_of(found: Found, elapsed: Duration) -> Usage {
    let mut usage = Usage {
        files: found.files,
        skipped: found.skipped,
        elapsed,
        memo: Memo {
            dirs: found.memo.into_iter().collect(),
        },
        ..Usage::default()
    };
    let mut top = found.top;
    let names = found.names;

    // Data with every name in here is the worktree's own, and deleting it
    // frees it: one name carries it as what it would be unlinked. Data with a
    // name outside is shared, and counted once towards what sharing spares.
    let mut found_here: HashMap<(u64, u64), u64> = HashMap::new();
    for name in &names {
        *found_here
            .entry((name.file.volume, name.file.id))
            .or_default() += 1;
    }
    let mut counted = HashSet::new();
    for name in &names {
        let key = (name.file.volume, name.file.id);
        if !counted.insert(key) {
            continue;
        }
        if found_here[&key] >= name.links {
            if let Some((_, split)) = top.get_mut(&name.top) {
                split.linked -= name.file.size;
                split.add(name.kind, name.file.size);
            }
        } else {
            usage.shared += name.file.size;
        }
    }
    usage.linked = names.into_iter().map(|name| name.file).collect();
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
    usage
}

/// Total size of a directory, without classifying anything.
pub fn tree_size(path: &Path) -> u64 {
    let progress = ScanProgress::default();
    let request = ScanRequest {
        root: path,
        skip: HashSet::new(),
        tracked: None,
        links: false,
        previous: None,
        progress: &progress,
    };
    scan(&request).map_or(0, |usage| usage.split.total())
}

/// What a set of worktrees takes, with hard-linked data counted once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// What they take on disk.
    pub on_disk: u64,
    /// Bytes that adding up file sizes counts more than once, because the
    /// same data has several names.
    pub repeated: u64,
    /// The part of that spared by worktrees sharing data: what each would
    /// take more with a copy of its own. Names Cargo makes inside one
    /// worktree's `target/` are not counted.
    pub between: u64,
}

/// What the given worktrees take, one report per worktree.
pub fn deduplicated<'a>(reports: impl IntoIterator<Item = &'a Usage>) -> Totals {
    let mut logical = 0u64;
    let mut linked_logical = 0u64;
    // Per piece of data: its size, in how many worktrees it was found, and
    // the last of them.
    let mut seen: HashMap<(u64, u64), (u64, u64, usize)> = HashMap::new();
    for (worktree, usage) in reports.into_iter().enumerate() {
        logical += usage.split.total();
        for file in &usage.linked {
            linked_logical += file.size;
            let slot = seen
                .entry((file.volume, file.id))
                .or_insert((file.size, 0, usize::MAX));
            if slot.2 != worktree {
                slot.1 += 1;
                slot.2 = worktree;
            }
        }
    }
    let linked_unique: u64 = seen.values().map(|(size, _, _)| size).sum();
    let between = seen
        .values()
        .map(|(size, worktrees, _)| size * (worktrees - 1))
        .sum();
    let repeated = linked_logical - linked_unique;
    Totals {
        on_disk: logical - repeated,
        repeated,
        between,
    }
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
            previous: None,
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
            previous: None,
            progress: &progress,
        })
        .unwrap();
        assert_eq!(usage_a.shared, 5000);
        assert_eq!(usage_b.split.linked, 5000);
        let totals = deduplicated([&usage_a, &usage_b]);
        assert_eq!(totals.repeated, 5000);
        assert_eq!(totals.between, 5000);
        assert_eq!(totals.on_disk, 100 + 1000 + 5000 + 10);
    }

    /// Trees measured together come out as they do alone, each reported once,
    /// and one that cannot be opened does not hold up the others.
    #[test]
    fn measures_trees_together_as_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (big, small) = (dir.path().join("big"), dir.path().join("small"));
        for index in 0..300 {
            write(
                &big.join(format!("d{}/e{}/f{index}.bin", index % 7, index % 13)),
                index,
            );
        }
        write(&small.join("readme.md"), 10);
        let missing = dir.path().join("missing");
        let progress = [(); 3].map(|_| ScanProgress::default());
        let requests: Vec<ScanRequest<'_>> = [&big, &small, &missing]
            .into_iter()
            .zip(&progress)
            .map(|(root, progress)| ScanRequest {
                root,
                skip: HashSet::new(),
                tracked: None,
                links: true,
                previous: None,
                progress,
            })
            .collect();

        let reports = Mutex::new(Vec::new());
        scan_all(&requests, |index, usage| {
            reports.lock().unwrap().push((index, usage));
        });
        let mut reports = reports.into_inner().unwrap();
        reports.sort_by_key(|(index, _)| *index);
        assert_eq!(reports.len(), 3);
        for (index, usage) in &reports[..2] {
            let together = usage.as_ref().unwrap();
            let alone = scan(&requests[*index]).unwrap();
            assert_eq!(together.files, alone.files);
            assert_eq!(together.split, alone.split);
        }
        assert_eq!(reports[0].1.as_ref().unwrap().files, 300);
        assert_eq!(reports[1].1.as_ref().unwrap().split.untracked, 10);
        assert!(reports[2].1.is_err());
    }

    /// A directory whose time has not moved is taken from the last scan
    /// without opening its files, so a link made to one of them from outside
    /// goes unseen; once an entry in it changes, it is read again. A memo
    /// comes back from its file as it went in, and only under its key.
    #[test]
    fn unchanged_directories_come_from_the_last_scan() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        let outside = dir.path().join("outside");
        write(&tree.join("a/x.bin"), 100);
        write(&outside.join("z.bin"), 40);
        fs::hard_link(outside.join("z.bin"), tree.join("a/y.bin")).unwrap();
        write(&tree.join("b/w.bin"), 7);
        // Directory times this fresh are not relied on.
        std::thread::sleep(SETTLED + Duration::from_millis(200));

        let progress = ScanProgress::default();
        let measure = |previous: Option<&Memo>| {
            scan(&ScanRequest {
                root: &tree,
                skip: HashSet::new(),
                tracked: None,
                links: true,
                previous,
                progress: &progress,
            })
            .unwrap()
        };
        let first = measure(None);
        assert_eq!(first.split.linked, 40);
        assert_eq!(first.split.untracked, 107);

        // A link from outside moves no time in the tree.
        fs::hard_link(tree.join("a/x.bin"), outside.join("x.bin")).unwrap();
        let reused = measure(Some(&first.memo));
        assert_eq!(reused.split, first.split);
        assert_eq!(reused.linked, first.linked);
        let full = measure(None);
        assert_eq!(full.split.linked, 140);

        // A new entry moves the directory's time: it is read again.
        write(&tree.join("a/new.bin"), 3);
        let again = measure(Some(&first.memo));
        assert_eq!(again.split.linked, 140);
        assert_eq!(again.split.untracked, 10);

        let file = dir.path().join("memo.bin");
        full.memo.save(&file, "worktrees").unwrap();
        assert_eq!(Memo::load(&file, "worktrees").as_ref(), Some(&full.memo));
        assert_eq!(Memo::load(&file, "other worktrees"), None);
        let data = fs::read(&file).unwrap();
        fs::write(&file, &data[..data.len() - 1]).unwrap();
        assert_eq!(Memo::load(&file, "worktrees"), None);
    }

    /// Cargo gives a build output a second name in the same `target/`: that
    /// data is the worktree's own until another worktree links to it too.
    #[test]
    fn data_named_twice_inside_one_worktree_is_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("c");
        let d = dir.path().join("d");
        write(&c.join("target/debug/deps/tool-1234.exe"), 1000);
        fs::hard_link(
            c.join("target/debug/deps/tool-1234.exe"),
            c.join("target/debug/tool.exe"),
        )
        .unwrap();
        let progress = ScanProgress::default();
        let measure = |root: &Path| {
            scan(&ScanRequest {
                root,
                skip: HashSet::new(),
                tracked: None,
                links: true,
                previous: None,
                progress: &progress,
            })
            .unwrap()
        };

        let alone = measure(&c);
        assert_eq!(alone.split.untracked, 1000);
        assert_eq!(alone.split.linked, 1000);
        assert_eq!(alone.shared, 0);
        assert_eq!(alone.entries[0].split.untracked, 1000);
        let totals = deduplicated([&alone]);
        assert_eq!(totals.on_disk, 1000);
        assert_eq!(totals.repeated, 1000);
        assert_eq!(totals.between, 0);

        // Carried into another worktree, it is shared, and spared once.
        write(&d.join("readme.md"), 10);
        fs::create_dir_all(d.join("target/debug")).unwrap();
        fs::hard_link(
            c.join("target/debug/tool.exe"),
            d.join("target/debug/tool.exe"),
        )
        .unwrap();
        let (c, d) = (measure(&c), measure(&d));
        assert_eq!(c.split.untracked, 0);
        assert_eq!(c.split.linked, 2000);
        assert_eq!(c.shared, 1000);
        assert_eq!(d.split.linked, 1000);
        assert_eq!(d.shared, 1000);
        let totals = deduplicated([&c, &d]);
        assert_eq!(totals.on_disk, 1000 + 10);
        assert_eq!(totals.repeated, 2000);
        assert_eq!(totals.between, 1000);
    }
}
