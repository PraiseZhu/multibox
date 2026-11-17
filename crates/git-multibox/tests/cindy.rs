//! Runtime leases protect even a clean, already merged worktree, including
//! when a session started after the UI's last status snapshot.

use multibox_core::git;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn sh(dir: &Path, args: &[&str]) -> String {
    git::output(dir, args).unwrap_or_else(|err| panic!("git {args:?}: {err}"))
}

struct Fixture {
    _temp: TempDir,
    main: PathBuf,
    task: PathBuf,
    app_data: PathBuf,
    lease: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let main = temp.path().join("main");
        let task = temp.path().join("task");
        let app_data = temp.path().join("app-data");
        fs::create_dir(&main).unwrap();
        sh(&main, &["init", "-q", "-b", "trunk"]);
        sh(&main, &["config", "user.email", "test@example.com"]);
        sh(&main, &["config", "user.name", "test"]);
        fs::write(main.join("README.md"), "initial\n").unwrap();
        sh(&main, &["add", "."]);
        sh(&main, &["commit", "-qm", "initial"]);
        sh(
            &main,
            &["worktree", "add", "-qb", "task", task.to_str().unwrap()],
        );
        let lease = app_data
            .join("CindyGlobal-dev2-dev/worktree-runtime-leases")
            .join(format!("{}-{}.json", std::process::id(), "a".repeat(64)));
        fs::create_dir_all(lease.parent().unwrap()).unwrap();
        Self {
            _temp: temp,
            main,
            task,
            app_data,
            lease,
        }
    }

    fn occupy(&self) {
        fs::write(
            &self.lease,
            json!({
                "version": 1, "pid": std::process::id(), "path": self.task,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        // The child has its own profile environment; parallel tests and the
        // real Cindy profile are never changed.
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-multibox"));
        command
            .current_dir(&self.main)
            .args(args)
            .env("APPDATA", &self.app_data)
            .env("XDG_CONFIG_HOME", &self.app_data)
            .env(
                "XDT_USER_DATA_DIR",
                self.app_data.join("CindyGlobal-dev2-dev"),
            )
            .env_remove("CINDY_HOST_DATA_DIR");
        #[cfg(target_os = "macos")]
        command.env("HOME", &self.app_data);
        command.output().unwrap()
    }

    fn assert_refused(&self, args: &[&str]) {
        let before = sh(&self.main, &["rev-parse", "HEAD"]);
        let output = self.run(args);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {error}");
        assert!(error.contains("Cindy session"), "{args:?}: {error}");
        assert!(self.task.join("README.md").is_file());
        assert_eq!(sh(&self.main, &["rev-parse", "HEAD"]), before);
        assert!(
            !sh(&self.main, &["branch", "--list", "task"])
                .trim()
                .is_empty()
        );
        assert!(
            git::worktrees(&self.main)
                .unwrap()
                .iter()
                .any(|entry| entry.path.file_name() == Some("task".as_ref()))
        );
        assert!(
            !self
                .task
                .parent()
                .unwrap()
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".multibox-trash-"))
        );
    }
}

#[test]
fn clean_merged_worktree_cannot_be_removed_while_cindy_uses_it() {
    let fixture = Fixture::new();
    assert!(sh(&fixture.task, &["status", "--porcelain"]).is_empty());
    fixture.occupy();
    fixture.assert_refused(&["remove", "--wait", "../task"]);
    fixture.assert_refused(&["finish", "--wait", "../task"]);
    // Stopping one task releases its lease; normal cleanup works again.
    fs::remove_file(&fixture.lease).unwrap();
    let output = fixture.run(&["finish", "--wait", "../task"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.task.exists());
    assert!(
        sh(&fixture.main, &["branch", "--list", "task"])
            .trim()
            .is_empty()
    );
}

#[test]
fn force_cannot_discard_an_active_sessions_worktree() {
    let fixture = Fixture::new();
    fs::write(fixture.task.join("README.md"), "agent is editing\n").unwrap();
    fixture.occupy();
    fixture.assert_refused(&["remove", "--force", "--wait", "../task"]);
    assert_eq!(
        fs::read_to_string(fixture.task.join("README.md")).unwrap(),
        "agent is editing\n"
    );
    sh(&fixture.main, &["worktree", "lock", "../task"]);
    fixture.assert_refused(&["remove", "-f", "-f", "--wait", "../task"]);
}

#[test]
fn finish_refuses_before_merging_an_active_sessions_branch() {
    let fixture = Fixture::new();
    fs::write(fixture.task.join("feature"), "new commit\n").unwrap();
    sh(&fixture.task, &["add", "."]);
    sh(&fixture.task, &["commit", "-qm", "feature"]);
    fixture.occupy();
    fixture.assert_refused(&["finish", "--wait", "../task"]);
    assert!(!fixture.main.join("feature").exists());
}
