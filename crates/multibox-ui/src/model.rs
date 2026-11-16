//! What the window shows about a repository, gathered without changing it.
//!
//! [`load`] is quick: the worktree list, the volumes and leftover trash. The
//! slower facts, [`details`] from git and [`crate::usage`] from the disk,
//! arrive afterwards, one worktree at a time.

use multibox_clone::Volume;
use multibox_core::git::{self, Repo};
use multibox_core::index::Index;
use multibox_core::{Config, adapter};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Worktree {
    pub path: PathBuf,
    /// The folder name, which is how people tell worktrees apart.
    pub name: String,
    /// Short branch name, absent when HEAD is detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_main: bool,
    pub locked: bool,
    /// git no longer finds the directory; `git worktree prune` forgets it.
    pub prunable: bool,
    pub exists: bool,
}

impl Worktree {
    pub fn label(&self) -> String {
        match (&self.branch, &self.head) {
            (Some(branch), _) => branch.clone(),
            (None, Some(head)) => format!("detached @ {}", short_hash(head)),
            (None, None) => "(no commit)".to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The main worktree, which receives merged branches.
    pub main: PathBuf,
    /// Branch checked out in the main worktree.
    pub into: Option<String>,
    pub worktrees: Vec<Worktree>,
    pub volumes: Vec<Volume>,
    /// Directories a removal moved aside and has not finished deleting.
    pub trash: Vec<PathBuf>,
}

impl Snapshot {
    pub fn volume_of(&self, path: &Path) -> Option<&Volume> {
        let text = path.to_string_lossy().replace('\\', "/").to_lowercase();
        self.volumes
            .iter()
            .filter(|volume| {
                let root = volume.root.replace('\\', "/").to_lowercase();
                text.starts_with(&root) || text.starts_with(root.trim_end_matches('/'))
            })
            .max_by_key(|volume| volume.root.len())
    }
}

pub fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(8)]
}

fn short_branch(reference: &str) -> String {
    reference
        .strip_prefix("refs/heads/")
        .unwrap_or(reference)
        .to_owned()
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub fn load(dir: &Path) -> Result<Snapshot, String> {
    let list = git::worktrees(dir).map_err(|err| err.to_string())?;
    let mut worktrees: Vec<Worktree> = list
        .iter()
        .enumerate()
        .filter(|(_, info)| !info.bare)
        .map(|(position, info)| Worktree {
            name: folder_name(&info.path),
            branch: info.branch.as_deref().map(short_branch),
            head: info.head.clone(),
            is_main: position == 0,
            locked: info.locked,
            prunable: info.prunable,
            exists: info.path.is_dir(),
            path: info.path.clone(),
        })
        .collect();
    // A bare repository has no main worktree; the first linked one receives.
    if !worktrees.iter().any(|worktree| worktree.is_main)
        && let Some(first) = worktrees.first_mut()
    {
        first.is_main = true;
    }
    let main = worktrees
        .iter()
        .find(|worktree| worktree.is_main)
        .map(|worktree| worktree.path.clone())
        .ok_or_else(|| "this repository has no worktree with files".to_owned())?;
    let into = git::output(&main, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    Ok(Snapshot {
        trash: multibox_core::sweep_trash(&main),
        volumes: multibox_clone::volumes(),
        main,
        into,
        worktrees,
    })
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub time: i64,
    pub subject: String,
}

/// Facts from git and the engine adapters about one worktree.
#[derive(Clone, Debug, Default)]
pub struct Details {
    pub changed: usize,
    pub untracked: usize,
    /// Commits on this worktree's HEAD that the main branch lacks.
    pub ahead: Option<u32>,
    /// Commits on the main branch that this worktree lacks.
    pub behind: Option<u32>,
    pub last_commit: Option<Commit>,
    pub engines: Vec<String>,
    pub editor: Option<String>,
    pub filesystem: Option<String>,
    pub reflink: bool,
    pub merge_in_progress: bool,
    pub problems: Vec<String>,
}

impl Details {
    pub fn dirty(&self) -> bool {
        self.changed + self.untracked > 0
    }
}

pub fn details(worktree: &Worktree, main: &Path, into: Option<&str>) -> Details {
    let mut details = Details::default();
    if !worktree.exists {
        return details;
    }
    let path = &worktree.path;

    match git::output(path, &["status", "--porcelain", "--ignore-submodules=none"]) {
        Ok(status) => {
            for line in status.lines() {
                if line.starts_with("??") {
                    details.untracked += 1;
                } else if !line.is_empty() {
                    details.changed += 1;
                }
            }
        }
        Err(err) => details.problems.push(format!("git status: {err}")),
    }

    if let Ok(log) = git::output(path, &["log", "-1", "--format=%ct%x1f%s"])
        && let Some((time, subject)) = log.trim_end().split_once('\u{1f}')
    {
        details.last_commit = time.parse().ok().map(|time| Commit {
            time,
            subject: subject.to_owned(),
        });
    }

    if let (Some(into), Some(head)) = (into, worktree.head.as_deref())
        && !worktree.is_main
    {
        let range = format!("{into}...{head}");
        if let Ok(counts) = git::output(main, &["rev-list", "--left-right", "--count", &range]) {
            let mut numbers = counts.split_whitespace().map(str::parse::<u32>);
            if let (Some(Ok(behind)), Some(Ok(ahead))) = (numbers.next(), numbers.next()) {
                details.behind = Some(behind);
                details.ahead = Some(ahead);
            }
        }
    }

    if let Ok(capability) = multibox_clone::probe(path, path) {
        details.filesystem = Some(capability.filesystem);
        details.reflink = capability.reflink;
    }

    if let Ok(repo) = Repo::discover(path) {
        details.merge_in_progress = repo.git_dir.join("MERGE_HEAD").exists();
        if let Ok(index) = Index::read(&repo.index_path(), repo.hash_len) {
            let tracked: Vec<&str> = index.entries.iter().map(|e| e.path.as_str()).collect();
            let config = Config::load(path);
            for project in adapter::detect(path, &tracked) {
                let root = if project.root.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", project.root)
                };
                let version = project
                    .version
                    .as_deref()
                    .map(|version| format!(" {version}"))
                    .unwrap_or_default();
                let off = if config.engine_enabled(project.engine) {
                    ""
                } else {
                    " (off)"
                };
                details.engines.push(format!(
                    "{}{version}{root}{off}",
                    engine_name(project.engine)
                ));
                if details.editor.is_none()
                    && let Some(editor) = adapter::running_editor(path, &project)
                {
                    details.editor = Some(match editor.process_id {
                        Some(pid) => {
                            format!("{} 编辑器 (pid {pid})", engine_name(project.engine))
                        }
                        None => format!("{} 编辑器", engine_name(project.engine)),
                    });
                }
            }
        }
    }
    details
}

pub fn engine_name(engine: &str) -> &str {
    match engine {
        "unity" => "Unity",
        "cargo" => "Rust",
        "pnpm" => "pnpm",
        "typescript" => "TypeScript",
        other => other,
    }
}
