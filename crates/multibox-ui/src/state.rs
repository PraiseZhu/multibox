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
    Some(base.join("multibox/last-repository.json"))
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
