//! Cindy publishes a runtime lease while an agent (or its detached children)
//! can still use a worktree. Read that evidence without changing Cindy's state.

use crate::{Error, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const LEASES: &str = "worktree-runtime-leases";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn app_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return env_path("APPDATA")
        .or_else(|| env_path("USERPROFILE").map(|home| home.join("AppData/Roaming")));
    #[cfg(target_os = "macos")]
    return env_path("HOME").map(|home| home.join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    return env_path("XDG_CONFIG_HOME")
        .or_else(|| env_path("HOME").map(|home| home.join(".config")));
}

fn entries(dir: &Path) -> io::Result<Vec<fs::DirEntry>> {
    match fs::read_dir(dir) {
        Ok(entries) => entries.collect(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

fn lease_directories(app_data: Option<&Path>, explicit: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let mut directories: Vec<_> = explicit
        .iter()
        .map(|profile| profile.join(LEASES))
        .collect();
    if let Some(app_data) = app_data {
        // Both regional editions, development profiles, and the older shared
        // directory can hold leases for the very same physical worktree.
        directories.push(app_data.join("Cindy/shared-worktree-runtime-leases"));
        for entry in entries(app_data)? {
            if entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("cindy")
            {
                let profile = entry.path();
                if profile.is_dir() {
                    directories.push(profile.join(LEASES));
                }
            }
        }
        for region in ["cn", "global"] {
            directories.push(app_data.join("CindyHost").join(region).join(LEASES));
        }
    }
    directories.sort();
    directories.dedup();
    Ok(directories)
}

fn lease_name(name: &str) -> bool {
    let Some((pid, hash)) = name
        .strip_suffix(".json")
        .and_then(|name| name.split_once('-'))
    else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && hash.len() == 64
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn path_key(path: &Path) -> String {
    let physical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = physical.to_string_lossy().into_owned();
    #[cfg(windows)]
    return text
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase();
    #[cfg(not(windows))]
    return text;
}

fn in_use_in(worktree: &Path, directories: &[PathBuf]) -> Result<bool> {
    let target = path_key(worktree);
    for directory in directories {
        for entry in entries(directory).map_err(|err| {
            Error::new(format!(
                "cannot read Cindy leases in {}: {err}",
                directory.display()
            ))
        })? {
            if !lease_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let file = entry.path();
            let bytes = match fs::read(&file) {
                Ok(bytes) => bytes,
                // A session can release its lease while the directory is read.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(Error::new(format!(
                        "cannot read Cindy lease {}: {err}",
                        file.display()
                    )));
                }
            };
            let lease = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
            let path = lease
                .as_ref()
                .and_then(|lease| {
                    (lease["version"].as_u64() == Some(1))
                        .then(|| lease["path"].as_str())
                        .flatten()
                        .filter(|path| Path::new(path).is_absolute())
                })
                .ok_or_else(|| {
                    Error::new(format!(
                        "unreadable Cindy lease {}; cannot safely remove a worktree",
                        file.display()
                    ))
                })?;
            if path_key(Path::new(path)) == target {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether Cindy still holds a runtime lease for this physical worktree.
///
/// A dead owner's lease can still protect a detached child. Only Cindy knows
/// when that I/O has stopped, so neither PID checks nor age waive a lease here.
/// Missing profiles are fine; unreadable evidence must not authorize deletion.
pub fn cindy_worktree_in_use(worktree: &Path) -> Result<bool> {
    let explicit: Vec<_> = ["XDT_USER_DATA_DIR", "CINDY_HOST_DATA_DIR"]
        .iter()
        .filter_map(|name| env_path(name))
        .collect();
    let directories = lease_directories(app_data_dir().as_deref(), &explicit)?;
    in_use_in(worktree, &directories)
}

pub(crate) fn refuse_if_in_use(worktree: &Path) -> Result<()> {
    if cindy_worktree_in_use(worktree)? {
        return Err(Error::new(format!(
            "a Cindy session is using {}; stop the session and wait for its worktree lease to be released before removing it",
            worktree.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn lease(directory: &Path, path: &Path) -> PathBuf {
        fs::create_dir_all(directory).unwrap();
        let file = directory.join(format!("1-{}.json", "a".repeat(64)));
        fs::write(
            &file,
            json!({ "version": 1, "pid": 1, "path": path }).to_string(),
        )
        .unwrap();
        file
    }

    #[test]
    fn occupied_worktree_stays_protected_until_the_last_profile_releases_it() {
        let temp = TempDir::new().unwrap();
        let worktree = temp.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let directories = [
            temp.path().join("Cindy").join(LEASES),
            temp.path().join("CindyGlobal-dev2-dev").join(LEASES),
        ];
        assert!(!in_use_in(&worktree, &directories).unwrap());
        let first = lease(&directories[0], &worktree);
        let second = lease(&directories[1], &worktree.join("."));
        assert!(in_use_in(&worktree, &directories).unwrap());
        assert!(!in_use_in(temp.path(), &directories).unwrap());
        fs::remove_file(first).unwrap();
        assert!(in_use_in(&worktree, &directories).unwrap());
        fs::remove_file(second).unwrap();
        assert!(!in_use_in(&worktree, &directories).unwrap());
    }

    #[test]
    fn partial_leases_block_removal_but_release_receipts_are_not_leases() {
        let temp = TempDir::new().unwrap();
        let file = lease(temp.path(), temp.path());
        fs::write(&file, "{\"version\":").unwrap();
        assert!(in_use_in(temp.path(), &[temp.path().to_path_buf()]).is_err());
        fs::rename(&file, file.with_extension("json.release")).unwrap();
        assert!(!in_use_in(temp.path(), &[temp.path().to_path_buf()]).unwrap());
    }

    #[test]
    fn discovers_regional_development_shared_host_and_explicit_profiles() {
        let temp = TempDir::new().unwrap();
        for name in ["Cindy", "CindyGlobal", "CindyGlobal-dev2-dev", "unrelated"] {
            fs::create_dir(temp.path().join(name)).unwrap();
        }
        let custom = temp.path().join("custom-profile");
        let directories =
            lease_directories(Some(temp.path()), std::slice::from_ref(&custom)).unwrap();
        for profile in [
            "Cindy",
            "CindyGlobal",
            "CindyGlobal-dev2-dev",
            "CindyHost/cn",
            "CindyHost/global",
        ] {
            assert!(directories.contains(&temp.path().join(profile).join(LEASES)));
        }
        assert!(directories.contains(&temp.path().join("Cindy/shared-worktree-runtime-leases")));
        assert!(directories.contains(&custom.join(LEASES)));
        assert!(!directories.contains(&temp.path().join("unrelated").join(LEASES)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_match_across_case_slashes_and_extended_prefix() {
        let temp = TempDir::new().unwrap();
        let canonical = fs::canonicalize(temp.path()).unwrap();
        let reported = canonical
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('\\', "/")
            .to_uppercase();
        lease(temp.path(), Path::new(&reported));
        assert!(in_use_in(&canonical, &[temp.path().to_path_buf()]).unwrap());
    }
}
