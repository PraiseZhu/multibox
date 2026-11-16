use crate::scan::{Decision, ScanRequest, scan};
use crate::{join, thread_count};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Debug, Default)]
pub struct PurgeReport {
    pub files: u64,
    pub dirs: u64,
    pub failures: Vec<String>,
}

/// Treats an entry that is already gone as done: another purge of the same
/// directory may be running after an interrupted removal.
fn tolerate_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Removes a file or a link. A directory link needs `remove_dir` on Windows.
///
/// A read-only file is made writable and removed at once: a whole tree of
/// them, as a Perforce workspace has, must not wait. Anything else that fails
/// is retried a few times, as for directories: on Windows a file just written
/// is often held for a moment by an indexer or a virus scanner, and removing
/// it then fails with access denied or a sharing violation.
fn remove_entry(path: &Path) -> io::Result<()> {
    let once = || match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => fs::remove_dir(path).map_err(|_| err),
        done => done,
    };
    let mut result = once();
    if matches!(&result, Err(err) if err.kind() == io::ErrorKind::PermissionDenied)
        && make_writable(path).unwrap_or(false)
    {
        result = once();
    }
    let mut delay = Duration::from_millis(20);
    for _ in 0..6 {
        match &result {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                std::thread::sleep(delay);
                delay *= 2;
                result = once();
            }
            _ => break,
        }
    }
    result
}

/// Clears the read-only flag. Whether there was one to clear.
fn make_writable(path: &Path) -> io::Result<bool> {
    let mut permissions = match fs::symlink_metadata(path) {
        Ok(meta) => meta.permissions(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if !permissions.readonly() {
        return Ok(false);
    }
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)?;
    Ok(true)
}

/// Removes an empty directory. On Windows a file deleted while another process
/// has it open, an indexer or a virus scanner for instance, lingers until that
/// handle closes, and so does a handle on the directory itself; both make the
/// removal fail for a moment. A few short retries ride that out.
fn remove_dir_patiently(path: &Path) -> io::Result<()> {
    let mut delay = Duration::from_millis(20);
    for _ in 0..6 {
        match tolerate_missing(fs::remove_dir(path)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                std::thread::sleep(delay);
                delay *= 2;
            }
            done => return done,
        }
    }
    tolerate_missing(fs::remove_dir(path))
}

/// Deletes `root` and everything below it, in parallel. Links are removed, never
/// followed, so a junction inside the tree cannot reach data outside it.
pub fn purge(root: &Path, threads: Option<usize>) -> io::Result<PurgeReport> {
    let threads = thread_count(threads);
    let everything = |_: &str, _: bool| Decision::Include;
    let manifest = scan(
        root,
        &ScanRequest {
            files: Vec::new(),
            links: Vec::new(),
            trees: vec![String::new()],
            filter: &everything,
            threads: Some(threads),
        },
    )?;
    let root = fs::canonicalize(root)?;
    let failures = Mutex::new(Vec::new());

    let leaves: Vec<&str> = manifest
        .files
        .iter()
        .map(|file| file.rel.as_str())
        .chain(manifest.links.iter().map(|link| link.rel.as_str()))
        .collect();
    let cursor = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let start = cursor.fetch_add(64, Ordering::Relaxed);
                    if start >= leaves.len() {
                        return;
                    }
                    for rel in &leaves[start..(start + 64).min(leaves.len())] {
                        let path = join(&root, rel);
                        if let Err(err) = tolerate_missing(remove_entry(&path)) {
                            failures.lock().unwrap().push(format!("{rel}: {err}"));
                        }
                    }
                }
            });
        }
    });

    let mut dirs: Vec<&str> = manifest.dirs.iter().map(|dir| dir.rel.as_str()).collect();
    dirs.sort_unstable_by_key(|rel| std::cmp::Reverse(rel.len()));
    for rel in dirs.iter().copied().chain([""]) {
        let path = if rel.is_empty() {
            root.clone()
        } else {
            join(&root, rel)
        };
        if let Err(err) = remove_dir_patiently(&path) {
            let rel = if rel.is_empty() { "." } else { rel };
            failures.lock().unwrap().push(format!("{rel}: {err}"));
        }
    }

    Ok(PurgeReport {
        files: leaves.len() as u64,
        dirs: dirs.len() as u64,
        failures: failures.into_inner().unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scanner opening a file just written, as Windows Defender does, holds
    /// it without sharing deletion: removing the file fails until it lets go.
    #[cfg(windows)]
    #[test]
    fn a_file_held_for_a_moment_is_still_removed() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("trash");
        fs::create_dir_all(root.join("Library")).unwrap();
        fs::write(root.join("Library/ArtifactDB"), b"artifact db").unwrap();
        fs::write(root.join("readme.md"), b"readme").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(root.join("Library/ArtifactDB"))
            .unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
        });
        let report = purge(&root, Some(2)).unwrap();
        releaser.join().unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!root.exists());
    }

    #[test]
    fn read_only_files_and_directory_links_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("trash");
        let outside = dir.path().join("outside");
        fs::create_dir_all(root.join("depot")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("kept.txt"), b"kept").unwrap();
        let file = root.join("depot/checked-in.txt");
        fs::write(&file, b"read only").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&outside, root.join("link")).ok();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let report = purge(&root, Some(2)).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!root.exists());
        // The link was removed, not followed.
        assert!(outside.join("kept.txt").exists());
    }
}
