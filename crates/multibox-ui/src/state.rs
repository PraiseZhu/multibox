//! The last successfully opened repository, kept outside the repositories.

use multibox_core::git::Repo;
use std::fs;
use std::path::{Path, PathBuf};

pub fn startup_repository(argument: Option<&Path>, cwd: Option<&Path>) -> Option<PathBuf> {
    repository_from(argument, cwd, state_file().as_deref())
}

fn repository_from(
    argument: Option<&Path>,
    cwd: Option<&Path>,
    state: Option<&Path>,
) -> Option<PathBuf> {
    let discover = |path: &Path| Repo::discover(path).ok().map(|repo| repo.worktree);
    // An explicit path always wins, including an invalid one: do not silently
    // open a different repository when the caller asked for a particular one.
    if let Some(path) = argument {
        return discover(path);
    }
    cwd.and_then(discover).or_else(|| {
        let saved: PathBuf = serde_json::from_slice(&fs::read(state?).ok()?).ok()?;
        // Relative paths must not depend on where the next launch comes from.
        saved.is_absolute().then(|| discover(&saved)).flatten()
    })
}

pub fn remember_repository(repository: &Path) {
    if let Some(state) = state_file()
        && let Err(err) = write_repository(&state, repository)
    {
        eprintln!("multibox-ui: cannot remember the repository: {err}");
    }
}

fn write_repository(state: &Path, repository: &Path) -> std::io::Result<()> {
    let data = serde_json::to_vec(repository)?;
    if let Some(parent) = state.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut pending = tempfile::NamedTempFile::new_in(state.parent().unwrap_or(Path::new(".")))?;
    std::io::Write::write_all(&mut pending, &data)?;
    pending.persist(state)?;
    Ok(())
}

fn state_file() -> Option<PathBuf> {
    Some(state_dir()?.join("last-repository.json"))
}

fn state_dir() -> Option<PathBuf> {
    let env_path = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    #[cfg(windows)]
    let base = env_path("LOCALAPPDATA").or_else(|| env_path("APPDATA"))?;
    #[cfg(target_os = "macos")]
    let base = env_path("HOME")?.join("Library/Application Support");
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = env_path("XDG_STATE_HOME")
        .or_else(|| env_path("HOME").map(|home| home.join(".local/state")))?;
    Some(base.join("multibox"))
}

/// Where the last measurement of the worktree at `worktree` keeps what the
/// next one can reuse: one file per worktree, named by its path.
pub fn memo_file(worktree: &Path) -> Option<PathBuf> {
    Some(memo_dir()?.join(format!("{:016x}.bin", fnv1a(worktree))))
}

fn memo_dir() -> Option<PathBuf> {
    Some(state_dir()?.join("measured"))
}

/// A hash of a path that stays the same from one build to the next, which
/// the standard library's does not promise.
fn fnv1a(path: &Path) -> u64 {
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// Drops the memos of worktrees no measurement has used for a month: most
/// belong to worktrees long removed, and each can hold a few megabytes.
pub fn forget_old_memos() {
    const MONTH: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);
    let Some(entries) = memo_dir().and_then(|dir| fs::read_dir(dir).ok()) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > MONTH);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multibox_core::git;
    use tempfile::TempDir;

    fn repository(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        fs::create_dir(&dir).unwrap();
        git::quiet(&dir, &["init", "-q"]).unwrap();
        Repo::discover(&dir).unwrap().worktree
    }

    #[test]
    fn reopening_restores_the_last_repository_and_remembers_switches() {
        let temp = TempDir::new().unwrap();
        let first = repository(temp.path(), "第一个仓库 with spaces");
        let second = repository(temp.path(), "second");
        let state = temp.path().join("settings/last-repository.json");

        write_repository(&state, &first).unwrap();
        assert_eq!(
            repository_from(None, Some(temp.path()), Some(&state)),
            Some(first)
        );

        write_repository(&state, &second).unwrap();
        assert_eq!(repository_from(None, None, Some(&state)), Some(second));
    }

    #[test]
    fn explicit_and_current_repositories_take_priority_over_the_saved_one() {
        let temp = TempDir::new().unwrap();
        let saved = repository(temp.path(), "saved");
        let current = repository(temp.path(), "current");
        let explicit = repository(temp.path(), "explicit");
        let nested = explicit.join("subdirectory");
        fs::create_dir(&nested).unwrap();
        let state = temp.path().join("last-repository.json");
        write_repository(&state, &saved).unwrap();

        assert_eq!(
            repository_from(Some(&nested), Some(&current), Some(&state)),
            Some(explicit)
        );
        assert_eq!(
            repository_from(None, Some(&current), Some(&state)),
            Some(current)
        );
        assert_eq!(repository_from(Some(temp.path()), None, Some(&state)), None);
    }

    #[test]
    fn missing_invalid_and_stale_state_open_the_directory_picker() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("last-repository.json");
        assert_eq!(repository_from(None, None, Some(&state)), None);
        for data in ["{broken", "{}", "null", "\"relative/path\""] {
            fs::write(&state, data).unwrap();
            assert_eq!(repository_from(None, None, Some(&state)), None);
        }

        write_repository(&state, &temp.path().join("gone")).unwrap();
        assert_eq!(repository_from(None, None, Some(&state)), None);
        write_repository(&state, temp.path()).unwrap();
        assert_eq!(repository_from(None, None, Some(&state)), None);
    }
}
