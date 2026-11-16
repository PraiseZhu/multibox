//! The buttons' work. Each function blocks, so the window runs them on a
//! background thread, and each reports in a sentence what it did.

use multibox_core::{
    AddOptions, FinishOptions, FinishOutcome, Mode, Observer, Progress, RemoveOptions, git,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Deletes what a removal moved aside, now rather than in another process:
/// the window stays open, and shows when the space is back.
fn purge(trash: Option<PathBuf>) -> Vec<String> {
    let Some(trash) = trash else {
        return Vec::new();
    };
    match multibox_core::purge_trash(&trash) {
        Ok(report) => report.failures.into_iter().take(3).collect(),
        Err(err) => vec![err.to_string()],
    }
}

fn with_failures(message: String, failures: Vec<String>) -> String {
    if failures.is_empty() {
        message
    } else {
        format!("{message}；有文件没删掉：{}", failures.join("; "))
    }
}

/// `git multibox remove`, then the deferred delete in the foreground.
pub fn remove(main: &Path, path: &Path, force: bool, locked: bool) -> Result<String, String> {
    let force = match (force, locked) {
        (false, _) => 0,
        (true, false) => 1,
        (true, true) => 2,
    };
    let outcome = multibox_core::remove(
        main,
        &RemoveOptions {
            path: text(path),
            force,
        },
    )
    .map_err(|err| err.to_string())?;
    let failures = purge(outcome.trash);
    Ok(with_failures(
        format!("已删除 {}", outcome.path.display()),
        failures,
    ))
}

pub enum Finished {
    Done(String),
    Conflicts(String),
}

/// `git multibox finish`: merge the branch into the main worktree's branch,
/// then remove the worktree and the merged branch.
pub fn finish(main: &Path, path: &Path) -> Result<Finished, String> {
    let outcome = multibox_core::finish(
        main,
        &FinishOptions {
            path: text(path),
            keep_branch: false,
            merge_args: Vec::new(),
        },
    )
    .map_err(|err| err.to_string())?;
    match outcome {
        FinishOutcome::Finished {
            branch,
            into,
            merged_now,
            removal,
            branch_deleted,
            ..
        } => {
            let failures = purge(removal.trash);
            let how = if merged_now {
                "已合并到"
            } else {
                "早已合并到"
            };
            let deleted = if branch_deleted {
                "，分支已删除"
            } else {
                ""
            };
            Ok(Finished::Done(with_failures(
                format!(
                    "'{branch}' {how} '{into}'，已删除 {}{deleted}",
                    removal.path.display()
                ),
                failures,
            )))
        }
        FinishOutcome::Conflicts {
            branch,
            into,
            files,
        } => Ok(Finished::Conflicts(format!(
            "把 '{branch}' 合并到 '{into}' 时有冲突，合并停在主工作区里等你处理：\n{}\n解决并提交后再点一次“合并并清理”；或者在主工作区运行 git merge --abort 撤销。",
            files.join("\n")
        ))),
    }
}

pub fn set_locked(main: &Path, path: &Path, lock: bool) -> Result<String, String> {
    let target = text(path);
    let verb = if lock { "lock" } else { "unlock" };
    let mut args = vec!["worktree", verb];
    if lock {
        args.extend(["--reason", "locked from Multibox"]);
    }
    args.push(&target);
    git::quiet(main, &args).map_err(|err| err.to_string())?;
    Ok(if lock {
        format!("已锁定 {target}")
    } else {
        format!("已解锁 {target}")
    })
}

/// `git worktree prune`: forget worktrees whose directory is gone.
pub fn prune(main: &Path) -> Result<String, String> {
    // git reports what it prunes on stderr, so count the list instead.
    let listed = || {
        git::worktrees(main)
            .map(|list| list.len())
            .map_err(|err| err.to_string())
    };
    let before = listed()?;
    git::quiet(main, &["worktree", "prune"]).map_err(|err| err.to_string())?;
    let count = before.saturating_sub(listed()?);
    Ok(if count == 0 {
        "没有需要清理的失效记录".to_owned()
    } else {
        format!("已清除 {count} 条失效的 worktree 记录")
    })
}

/// Finishes deleting every directory an earlier removal left behind.
pub fn purge_all_trash(main: &Path) -> Result<String, String> {
    let trash = multibox_core::sweep_trash(main);
    if trash.is_empty() {
        return Ok("没有残留的待删目录".to_owned());
    }
    let mut failures = Vec::new();
    for path in &trash {
        failures.extend(purge(Some(path.clone())));
    }
    Ok(with_failures(
        format!("已清空 {} 个残留的待删目录", trash.len()),
        failures,
    ))
}

/// Progress of a running `add`, read by the window while it works.
#[derive(Default)]
pub struct AddWatch {
    pub notes: Mutex<Vec<String>>,
    pub clone: Mutex<Option<(u64, u64, Arc<Progress>)>>,
}

impl Observer for AddWatch {
    fn note(&self, message: &str) {
        self.notes.lock().unwrap().push(message.to_owned());
    }

    fn warn(&self, message: &str) {
        self.notes.lock().unwrap().push(format!("注意：{message}"));
    }

    fn clone_started(&self, files: u64, bytes: u64, progress: Arc<Progress>) {
        *self.clone.lock().unwrap() = Some((files, bytes, progress));
    }
}

/// `git multibox add <path> [-b <branch>]` from the main worktree.
pub fn add(main: &Path, path: &str, branch: &str, watch: &AddWatch) -> Result<String, String> {
    let mut git_args = Vec::new();
    if !branch.is_empty() {
        git_args.extend(["-b".to_owned(), branch.to_owned()]);
    }
    git_args.push(path.to_owned());
    let outcome = multibox_core::add(
        main,
        &AddOptions {
            git_args,
            path: path.to_owned(),
            from: None,
            mode: Mode::Auto,
            carry: true,
            dry_run: false,
            threads: None,
        },
        watch,
    )
    .map_err(|err| err.to_string())?;
    let summary = outcome.summary;
    let how = if outcome.native {
        "普通 git worktree".to_owned()
    } else if outcome.capability.reflink {
        format!("{} 上 reflink 共享", outcome.capability.filesystem)
    } else {
        format!("{} 上复制", outcome.capability.filesystem)
    };
    let files = summary.tracked_files + summary.cached_files;
    let bytes = summary.tracked_bytes + summary.cached_bytes;
    Ok(format!(
        "已创建 {}：{} 个文件（{}），{how}",
        outcome.path.display(),
        crate::format::count(files),
        crate::format::bytes(bytes)
    ))
}

/// Removes a worktree whose branch is merged, then the branch. `git branch -d`
/// refuses a branch that is not merged after all, so nothing is lost if the
/// window's view of the repository was out of date.
pub fn clean_merged(main: &Path, path: &Path, branch: Option<&str>) -> Result<String, String> {
    let message = remove(main, path, false, false)?;
    if let Some(branch) = branch
        && git::quiet(main, &["branch", "-d", branch]).is_ok()
    {
        return Ok(format!("{message}，分支 '{branch}' 已删除"));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    //! Every button's work, against real repositories in temporary directories.

    use super::*;
    use crate::model;
    use std::fs;
    use tempfile::TempDir;

    fn sh(dir: &Path, args: &[&str]) -> String {
        git::output(dir, args).unwrap_or_else(|err| panic!("git {args:?}: {err}"))
    }

    fn repo() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let main = temp.path().join("main");
        fs::create_dir(&main).unwrap();
        sh(&main, &["init", "-q", "-b", "trunk"]);
        sh(&main, &["config", "user.email", "test@example.com"]);
        sh(&main, &["config", "user.name", "test"]);
        sh(&main, &["config", "core.autocrlf", "false"]);
        fs::write(main.join("README.md"), "readme\n").unwrap();
        fs::write(main.join(".gitignore"), "cache/\n").unwrap();
        sh(&main, &["add", "."]);
        sh(&main, &["commit", "-q", "-m", "init"]);
        fs::create_dir(main.join("cache")).unwrap();
        fs::write(main.join("cache/blob"), vec![0u8; 4096]).unwrap();
        (temp, main)
    }

    fn commit(dir: &Path, file: &str, message: &str) {
        fs::write(dir.join(file), message).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-q", "-m", message]);
    }

    fn worktree<'a>(snapshot: &'a model::Snapshot, name: &str) -> Option<&'a model::Worktree> {
        snapshot.worktrees.iter().find(|w| w.name == name)
    }

    #[test]
    fn add_then_finish_merges_and_cleans_up() {
        let (temp, main) = repo();
        let watch = AddWatch::default();
        let message = add(&main, "../task", "task", &watch).unwrap();
        assert!(message.contains("task"), "{message}");
        let task = temp.path().join("task");
        assert!(task.join("README.md").is_file());

        commit(&task, "feature.txt", "feature");
        let snapshot = model::load(&main).unwrap();
        let entry = worktree(&snapshot, "task").unwrap();
        let details = model::details(entry, &snapshot.main, snapshot.into.as_deref());
        assert_eq!((details.ahead, details.behind), (Some(1), Some(0)));
        assert!(!details.dirty());

        match finish(&main, &task).unwrap() {
            Finished::Done(message) => assert!(message.contains("task"), "{message}"),
            Finished::Conflicts(message) => panic!("{message}"),
        }
        assert!(!task.exists());
        assert!(main.join("feature.txt").is_file());
        assert!(sh(&main, &["branch", "--list", "task"]).trim().is_empty());
    }

    #[test]
    fn finish_stops_on_conflicts_and_keeps_the_worktree() {
        let (temp, main) = repo();
        add(&main, "../clash", "clash", &AddWatch::default()).unwrap();
        let clash = temp.path().join("clash");
        commit(&clash, "README.md", "theirs");
        commit(&main, "README.md", "ours");
        match finish(&main, &clash).unwrap() {
            Finished::Conflicts(message) => assert!(message.contains("README.md"), "{message}"),
            Finished::Done(message) => panic!("merged: {message}"),
        }
        assert!(clash.is_dir());
        sh(&main, &["merge", "--abort"]);
    }

    #[test]
    fn merged_worktrees_are_cleaned_with_their_branch() {
        let (temp, main) = repo();
        add(&main, "../done", "done", &AddWatch::default()).unwrap();
        let done = temp.path().join("done");
        let snapshot = model::load(&main).unwrap();
        let entry = worktree(&snapshot, "done").unwrap();
        let details = model::details(entry, &snapshot.main, snapshot.into.as_deref());
        assert_eq!(details.ahead, Some(0));

        let message = clean_merged(&main, &done, Some("done")).unwrap();
        assert!(message.contains("done"), "{message}");
        assert!(!done.exists());
        assert!(sh(&main, &["branch", "--list", "done"]).trim().is_empty());
    }

    #[test]
    fn an_unmerged_branch_survives_cleaning() {
        let (temp, main) = repo();
        add(&main, "../ahead", "ahead", &AddWatch::default()).unwrap();
        let ahead = temp.path().join("ahead");
        commit(&ahead, "more.txt", "more");
        clean_merged(&main, &ahead, Some("ahead")).unwrap();
        assert!(!ahead.exists());
        assert!(!sh(&main, &["branch", "--list", "ahead"]).trim().is_empty());
    }

    #[test]
    fn remove_refuses_dirty_and_locked_worktrees_unless_forced() {
        let (temp, main) = repo();
        add(&main, "../wip", "wip", &AddWatch::default()).unwrap();
        let wip = temp.path().join("wip");
        fs::write(wip.join("README.md"), "changed\n").unwrap();
        let err = remove(&main, &wip, false, false).unwrap_err();
        assert!(err.contains("modified"), "{err}");

        set_locked(&main, &wip, true).unwrap();
        let snapshot = model::load(&main).unwrap();
        assert!(worktree(&snapshot, "wip").unwrap().locked);
        assert!(
            remove(&main, &wip, true, false).is_err(),
            "locked needs -f -f"
        );
        remove(&main, &wip, true, true).unwrap();
        assert!(!wip.exists());
    }

    #[test]
    fn unlock_prune_and_trash() {
        let (temp, main) = repo();
        add(&main, "../gone", "gone", &AddWatch::default()).unwrap();
        let gone = temp.path().join("gone");
        set_locked(&main, &gone, true).unwrap();
        set_locked(&main, &gone, false).unwrap();
        assert!(
            !worktree(&model::load(&main).unwrap(), "gone")
                .unwrap()
                .locked
        );

        fs::remove_dir_all(&gone).unwrap();
        let snapshot = model::load(&main).unwrap();
        let entry = worktree(&snapshot, "gone").unwrap();
        assert!(entry.prunable && !entry.exists);
        assert_eq!(prune(&main).unwrap(), "已清除 1 条失效的 worktree 记录");
        assert!(worktree(&model::load(&main).unwrap(), "gone").is_none());

        let trash = temp.path().join(".multibox-trash-old-1");
        fs::create_dir_all(trash.join("deep")).unwrap();
        fs::write(trash.join("deep/file"), "x").unwrap();
        // By name: git reports the directory with links resolved, and the
        // temporary directory is behind one on macOS (/var → /private/var).
        let found = model::load(&main).unwrap().trash;
        let names: Vec<_> = found.iter().filter_map(|path| path.file_name()).collect();
        assert_eq!(names, [".multibox-trash-old-1"]);
        purge_all_trash(&main).unwrap();
        assert!(!trash.exists());
        assert!(model::load(&main).unwrap().trash.is_empty());
    }

    #[test]
    fn details_count_changes() {
        let (temp, main) = repo();
        add(&main, "../edit", "edit", &AddWatch::default()).unwrap();
        let edit = temp.path().join("edit");
        fs::write(edit.join("README.md"), "changed\n").unwrap();
        fs::write(edit.join("new.txt"), "new\n").unwrap();
        commit(&main, "main-only.txt", "main moves on");
        let snapshot = model::load(&main).unwrap();
        assert_eq!(snapshot.into.as_deref(), Some("trunk"));
        assert!(worktree(&snapshot, "main").unwrap().is_main);
        let entry = worktree(&snapshot, "edit").unwrap();
        let details = model::details(entry, &snapshot.main, snapshot.into.as_deref());
        assert_eq!((details.changed, details.untracked), (1, 1));
        assert_eq!((details.ahead, details.behind), (Some(0), Some(1)));
        assert_eq!(details.last_commit.as_ref().unwrap().subject, "init");
    }
}
