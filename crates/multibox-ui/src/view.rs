//! The window's state and what its buttons do. Drawing it is in [`render`].

mod render;

pub use render::{OWN_TITLE_BAR, TOP_BAR};

use crate::format;
use crate::model::{self, Details, Snapshot, Worktree};
use crate::ops::{self, AddWatch};
use crate::state;
use crate::theme::{self, Palette};
use crate::usage::{self, ScanProgress, ScanRequest, Usage};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{WindowExt, v_flex};
use gpui_kit::{
    AnyWindowHandle, AppContext, ClickEvent, ClipboardItem, Context, FocusHandle, Focusable,
    FontWeight, ParentElement, PathPromptOptions, SharedString, Styled, Subscription, Window, div,
    px,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

gpui_kit::actions!(multibox, [Refresh]);

/// What a finished action has to say.
enum Report {
    Success(String),
    Warning(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Remove,
    Finish,
    Clean,
    Lock,
}

/// How often git is asked again while the window is in front. Its answer for
/// a repository of a million files takes about a tenth of a second.
const POLL_EVERY: Duration = Duration::from_secs(30);

/// How old the last measurement must be before coming back to the window
/// measures again, from what changed since.
const REMEASURE_AFTER: Duration = Duration::from_secs(120);

/// Which worktrees a refresh measures, and how.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Measure {
    /// Only those not measured yet: after an operation, which changes git's
    /// state rather than what is on disk elsewhere.
    Missing,
    /// All of them, taking every directory unchanged since the last
    /// measurement from it: on opening a repository.
    Changed,
    /// All of them from scratch, trusting nothing kept: what refreshing by
    /// hand asks for.
    All,
}

/// Everything that can go without losing anything: worktrees whose branch is
/// merged, directories an interrupted removal left behind, and records of
/// worktrees whose directory is gone.
struct Cleanup {
    merged: Vec<Worktree>,
    trash: usize,
    stale: usize,
    bytes: u64,
}

impl Cleanup {
    fn is_empty(&self) -> bool {
        self.merged.is_empty() && self.trash == 0 && self.stale == 0
    }
}

pub struct MultiboxApp {
    repo: Option<PathBuf>,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    loading: bool,
    details: HashMap<PathBuf, Details>,
    /// Last measurement of each worktree, kept while it is measured again.
    usage: HashMap<PathBuf, Arc<Usage>>,
    scanning: HashMap<PathBuf, Arc<ScanProgress>>,
    scan_errors: HashMap<PathBuf, String>,
    trash_size: Option<u64>,
    /// What the measured worktrees take, hard-linked data counted once.
    dedup: usage::Totals,
    selected: Option<PathBuf>,
    busy: HashMap<PathBuf, Busy>,
    global_busy: Option<SharedString>,
    adding: Option<(String, Arc<AddWatch>)>,
    refreshed_at: Option<SystemTime>,
    generation: u64,
    ticking: bool,
    /// The left button went down on the top bar and the window has not been
    /// moved yet: macOS leaves moving a window without a system title bar to
    /// the app.
    moving_window: bool,
    /// The window is the one in front, taking the keyboard.
    active: bool,
    /// When every worktree was last measured, from scratch or from what
    /// changed.
    measured_at: Option<Instant>,
    window: AnyWindowHandle,
    focus: FocusHandle,
    _appearance: Subscription,
    _activation: Subscription,
}

impl MultiboxApp {
    pub fn new(repo: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // Follow the system between light and dark.
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
            cx.notify();
        });
        // Coming back to the window brings it up to date.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.active = window.is_window_active();
            if this.active {
                this.refresh_quietly(true, cx);
            }
        });
        let mut app = MultiboxApp {
            repo,
            snapshot: None,
            error: None,
            loading: false,
            details: HashMap::new(),
            usage: HashMap::new(),
            scanning: HashMap::new(),
            scan_errors: HashMap::new(),
            trash_size: None,
            dedup: usage::Totals::default(),
            selected: None,
            busy: HashMap::new(),
            global_busy: None,
            adding: None,
            refreshed_at: None,
            generation: 0,
            ticking: false,
            moving_window: false,
            active: window.is_window_active(),
            measured_at: None,
            window: window.window_handle(),
            focus,
            _appearance: appearance,
            _activation: activation,
        };
        app.refresh(Measure::Changed, cx);
        app.keep_up(cx);
        app
    }

    // ---- loading -------------------------------------------------------

    /// While the window is in front, asks git again every so often: what a
    /// worktree holds changes while it is looked at, and other tools add and
    /// remove worktrees.
    fn keep_up(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_EVERY).await;
                let alive = this.update(cx, |this, cx| {
                    if this.active {
                        this.refresh_quietly(false, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Brings what the window shows up to date without being asked: the
    /// worktrees and git's state of each, and with `remeasure` their sizes
    /// too, from what changed, once the last measurement is a while old.
    /// Never while something is under way: a refresh would cancel a
    /// measurement and start it over, and an operation refreshes when done.
    fn refresh_quietly(&mut self, remeasure: bool, cx: &mut Context<Self>) {
        let under_way = self.loading
            || !self.scanning.is_empty()
            || self.adding.is_some()
            || !self.busy.is_empty()
            || self.global_busy.is_some();
        if self.repo.is_none() || under_way {
            return;
        }
        let stale = self
            .measured_at
            .is_none_or(|at| at.elapsed() >= REMEASURE_AFTER);
        let kind = if remeasure && stale {
            Measure::Changed
        } else {
            Measure::Missing
        };
        self.refresh(kind, cx);
        // Nobody asked: the refresh button keeps still.
        self.loading = false;
    }

    /// Reloads the worktree list and git facts, and measures as `kind` says.
    fn refresh(&mut self, kind: Measure, cx: &mut Context<Self>) {
        let Some(dir) = self.repo.clone() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        for progress in self.scanning.values() {
            progress.cancel.store(true, Ordering::Relaxed);
        }
        self.scanning.clear();
        self.loading = true;
        self.error = None;
        cx.notify();
        let measured: HashSet<PathBuf> = match kind {
            Measure::Missing => self.usage.keys().cloned().collect(),
            Measure::Changed | Measure::All => HashSet::new(),
        };
        let reuse = kind != Measure::All;
        if kind != Measure::Missing {
            self.measured_at = Some(Instant::now());
        }

        cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { model::load(&dir) }).await;
            let snapshot = match loaded {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.generation == generation {
                            this.error = Some(err);
                            this.snapshot = None;
                            this.loading = false;
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let current = this
                .update(cx, |this, cx| {
                    if this.generation != generation {
                        return false;
                    }
                    if this.snapshot.is_none() {
                        // Keep the main worktree so removing a linked worktree
                        // does not leave the next launch pointing at it.
                        state::remember_repository(&snapshot.main);
                    }
                    let present: HashSet<&PathBuf> =
                        snapshot.worktrees.iter().map(|w| &w.path).collect();
                    this.details.retain(|path, _| present.contains(path));
                    this.usage.retain(|path, _| present.contains(path));
                    this.busy.retain(|path, _| present.contains(path));
                    this.scan_errors.clear();
                    if this
                        .selected
                        .as_ref()
                        .is_none_or(|selected| !present.contains(selected))
                    {
                        this.selected = snapshot.worktrees.first().map(|w| w.path.clone());
                    }
                    this.snapshot = Some(snapshot.clone());
                    this.refreshed_at = Some(SystemTime::now());
                    this.loading = false;
                    this.recompute_totals();
                    this.load_details(&snapshot, generation, cx);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !current {
                return;
            }

            // Every worktree at once, with one set of threads between them, so
            // a small one comes in long before a large one.
            let all: Vec<PathBuf> = snapshot
                .worktrees
                .iter()
                .filter(|worktree| worktree.exists)
                .map(|worktree| worktree.path.clone())
                .collect();
            let roots: Vec<PathBuf> = all
                .iter()
                .filter(|root| !measured.contains(*root))
                .cloned()
                .collect();
            if roots.is_empty() {
                return;
            }
            let progress: Vec<Arc<ScanProgress>> = roots.iter().map(|_| Arc::default()).collect();
            let current = this
                .update(cx, |this, cx| {
                    if this.generation != generation {
                        return false;
                    }
                    for (root, progress) in roots.iter().zip(&progress) {
                        this.scanning.insert(root.clone(), progress.clone());
                    }
                    this.start_ticker(cx);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let (sender, receiver) = async_channel::unbounded();
            let targets = roots.clone();
            cx.background_spawn(async move {
                measure(&targets, &all, &progress, reuse, |index, result| {
                    // Gone only when a newer refresh took over.
                    let _ = sender.send_blocking((index, result));
                });
            })
            .detach();
            while let Ok((index, result)) = receiver.recv().await {
                let root = &roots[index];
                let current = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        this.scanning.remove(root);
                        match result {
                            Ok(usage) => {
                                this.usage.insert(root.clone(), Arc::new(usage));
                                this.scan_errors.remove(root);
                            }
                            Err(err) => {
                                this.scan_errors.insert(root.clone(), err);
                            }
                        }
                        this.recompute_totals();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !current {
                    return;
                }
            }
        })
        .detach();
    }

    /// Git facts for every worktree at once, since they are independent, and
    /// the size of leftover trash.
    fn load_details(&mut self, snapshot: &Snapshot, generation: u64, cx: &mut Context<Self>) {
        let main = snapshot.main.clone();
        let into = snapshot.into.clone();
        let worktrees = snapshot.worktrees.clone();
        let trash = snapshot.trash.clone();
        cx.spawn(async move |this, cx| {
            let facts: Vec<_> = worktrees
                .into_iter()
                .map(|worktree| {
                    let (main, into) = (main.clone(), into.clone());
                    cx.background_spawn(async move {
                        let details = model::details(&worktree, &main, into.as_deref());
                        (worktree.path, details)
                    })
                })
                .collect();
            for fact in facts {
                let (path, details) = fact.await;
                let current = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        this.details.insert(path, details);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !current {
                    return;
                }
            }
            let trash_size = cx
                .background_spawn(async move { trash.iter().map(|p| usage::tree_size(p)).sum() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.trash_size = Some(trash_size);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn recompute_totals(&mut self) {
        let Some(snapshot) = &self.snapshot else {
            self.dedup = usage::Totals::default();
            return;
        };
        let reports = snapshot
            .worktrees
            .iter()
            .filter_map(|worktree| self.usage.get(&worktree.path))
            .map(|usage| usage.as_ref());
        self.dedup = usage::deduplicated(reports);
    }

    /// Redraws a few times a second while something reports progress.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let active = this
                    .update(cx, |this, cx| {
                        let active = !this.scanning.is_empty() || this.adding.is_some();
                        if active {
                            cx.notify();
                        } else {
                            this.ticking = false;
                        }
                        active
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
    }

    fn open_repo(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.repo = Some(path);
        self.snapshot = None;
        self.details.clear();
        self.usage.clear();
        self.scan_errors.clear();
        self.trash_size = None;
        self.selected = None;
        self.refreshed_at = None;
        self.refresh(Measure::Changed, cx);
    }

    fn pick_repo(&mut self, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("打开仓库".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = answer.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| this.open_repo(path, cx));
            }
        })
        .detach();
    }

    // ---- actions -------------------------------------------------------

    /// Runs `work` in the background, marks `key` (or the whole window) busy
    /// meanwhile, then reports and refreshes.
    fn run(
        &mut self,
        key: Option<(PathBuf, Busy)>,
        title: &'static str,
        work: impl FnOnce() -> Result<Report, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        match &key {
            Some((path, busy)) => {
                self.busy.insert(path.clone(), *busy);
            }
            None => self.global_busy = Some(title.into()),
        }
        cx.notify();
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { work() }).await;
            let note = match result {
                Ok(Report::Success(message)) => Notification::success(message),
                Ok(Report::Warning(message)) => {
                    Notification::warning(message).title(title).autohide(false)
                }
                Err(message) => Notification::error(message)
                    .title(format!("{title}失败"))
                    .autohide(false),
            };
            let _ = this.update(cx, |this, cx| {
                match &key {
                    Some((path, _)) => {
                        this.busy.remove(path);
                    }
                    None => this.global_busy = None,
                }
                this.refresh(Measure::Missing, cx);
            });
            let _ = window.update(cx, |_, window, cx| window.push_notification(note, cx));
        })
        .detach();
    }

    fn main_path(&self) -> Option<PathBuf> {
        self.snapshot.as_ref().map(|snapshot| snapshot.main.clone())
    }

    fn worktree(&self, path: &Path) -> Option<&Worktree> {
        self.snapshot
            .as_ref()?
            .worktrees
            .iter()
            .find(|worktree| worktree.path == path)
    }

    /// What deleting the worktree gives back: hard-linked data stays, since
    /// another worktree still holds it.
    fn freed(&self, path: &Path) -> Option<u64> {
        self.usage
            .get(path)
            .map(|usage| usage.split.total() - usage.split.linked)
    }

    /// On a cloning volume unchanged data may share blocks with other
    /// worktrees, which the file system does not report: the size is a bound.
    fn freed_text(&self, path: &Path) -> String {
        let bound = self
            .details
            .get(path)
            .is_some_and(|details| details.reflink);
        self.freed(path)
            .map(|bytes| {
                format!(
                    "，{}释放 {}",
                    if bound { "最多" } else { "" },
                    format::bytes(bytes)
                )
            })
            .unwrap_or_default()
    }

    fn confirm_remove(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree(&path).cloned() else {
            return;
        };
        let details = self.details.get(&path).cloned().unwrap_or_default();
        let force = details.dirty();
        let mut lines = vec![format!(
            "{} 会从磁盘上删除{}。",
            worktree.path.display(),
            self.freed_text(&path)
        )];
        if force {
            lines.push(format!(
                "里面还有 {} 处没提交的改动，删掉后找不回来。",
                details.changed + details.untracked
            ));
        }
        match details.ahead {
            Some(ahead) if ahead > 0 => lines.push(format!(
                "分支 {} 上有 {ahead} 个提交还没合并；分支会保留。",
                worktree.label()
            )),
            _ => {}
        }
        let description: SharedString = lines.join("\n").into();
        let title: SharedString = format!("删除工作区“{}”？", worktree.name).into();
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, path) = (view.clone(), path.clone());
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text(if force {
                    "连同改动一起删除"
                } else {
                    "删除"
                })
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    let path = path.clone();
                    view.update(cx, |this, cx| this.remove(path, force, cx));
                    true
                })
        });
    }

    fn remove(&mut self, path: PathBuf, force: bool, cx: &mut Context<Self>) {
        let (Some(main), Some(worktree)) = (self.main_path(), self.worktree(&path).cloned()) else {
            return;
        };
        let locked = worktree.locked;
        self.run(
            Some((path.clone(), Busy::Remove)),
            "删除工作区",
            move || ops::remove(&main, &path, force, locked).map(Report::Success),
            cx,
        );
    }

    fn confirm_finish(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(snapshot), Some(worktree)) =
            (self.snapshot.clone(), self.worktree(&path).cloned())
        else {
            return;
        };
        let details = self.details.get(&path).cloned().unwrap_or_default();
        let into = snapshot.into.clone().unwrap_or_else(|| "?".to_owned());
        let branch = worktree.branch.clone().unwrap_or_default();
        let description: SharedString = format!(
            "把 {branch} 的 {} 个提交合并进 {into}，然后删除这个工作区和分支{}。\n合并有冲突时会停下来，什么都不删。",
            details.ahead.unwrap_or(0),
            self.freed_text(&path)
        )
        .into();
        let title: SharedString = format!("合并“{}”并清理？", worktree.name).into();
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, path) = (view.clone(), path.clone());
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("合并并清理")
                .ok_variant(ButtonVariant::Primary)
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    let path = path.clone();
                    view.update(cx, |this, cx| this.finish(path, cx));
                    true
                })
        });
    }

    fn finish(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        self.run(
            Some((path.clone(), Busy::Finish)),
            "合并并清理",
            move || {
                ops::finish(&main, &path).map(|finished| match finished {
                    ops::Finished::Done(message) => Report::Success(message),
                    ops::Finished::Conflicts(message) => Report::Warning(message),
                })
            },
            cx,
        );
    }

    /// Whether all of the worktree's work is in the main branch and it holds
    /// nothing else, so removing it and its branch loses nothing.
    fn cleanable(&self, worktree: &Worktree) -> bool {
        let Some(details) = self.details.get(&worktree.path) else {
            return false;
        };
        details.cleanable(worktree)
    }

    fn cleanup(&self) -> Cleanup {
        let Some(snapshot) = &self.snapshot else {
            return Cleanup {
                merged: Vec::new(),
                trash: 0,
                stale: 0,
                bytes: 0,
            };
        };
        let merged: Vec<Worktree> = snapshot
            .worktrees
            .iter()
            .filter(|worktree| self.cleanable(worktree))
            .cloned()
            .collect();
        let freed: u64 = merged
            .iter()
            .filter_map(|worktree| self.freed(&worktree.path))
            .sum();
        Cleanup {
            trash: snapshot.trash.len(),
            stale: snapshot.worktrees.iter().filter(|w| w.prunable).count(),
            bytes: freed + self.trash_size.unwrap_or(0),
            merged,
        }
    }

    /// One button for everything that is safe to remove, after showing what
    /// that is.
    fn confirm_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cleanup = self.cleanup();
        if cleanup.is_empty() {
            return;
        }
        let mut lines = vec!["这些都可以放心清掉，不会丢任何东西：".to_owned()];
        for worktree in &cleanup.merged {
            lines.push(format!(
                "· 工作区 {} 和它的分支 {}：已经合并，没有未提交的改动{}",
                worktree.name,
                worktree.label(),
                self.freed_text(&worktree.path)
            ));
        }
        if cleanup.trash > 0 {
            lines.push(format!(
                "· {} 个没删完的残留目录，{}",
                cleanup.trash,
                format::bytes(self.trash_size.unwrap_or(0))
            ));
        }
        if cleanup.stale > 0 {
            lines.push(format!("· {} 条目录已经不在的工作区记录", cleanup.stale));
        }
        let description: SharedString = lines.join("\n").into();
        let title: SharedString = format!("清理 {}？", format::bytes(cleanup.bytes)).into();
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("清理")
                .ok_variant(ButtonVariant::Primary)
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    view.update(cx, |this, cx| this.clean_up(cx));
                    true
                })
        });
    }

    fn clean_up(&mut self, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        // Decided again now: the window may have been open for a while.
        let cleanup = self.cleanup();
        self.run(
            None,
            "清理",
            move || {
                let mut done = Vec::new();
                let mut failed = Vec::new();
                let mut merged = 0;
                for worktree in &cleanup.merged {
                    match ops::clean_merged(&main, &worktree.path, worktree.branch.as_deref()) {
                        Ok(_) => merged += 1,
                        Err(err) => failed.push(format!("{}：{err}", worktree.name)),
                    }
                }
                if merged > 0 {
                    done.push(format!("{merged} 个已合并的工作区"));
                }
                if cleanup.trash > 0 {
                    match ops::purge_all_trash(&main) {
                        Ok(_) => done.push(format!("{} 个残留目录", cleanup.trash)),
                        Err(err) => failed.push(format!("残留目录：{err}")),
                    }
                }
                if cleanup.stale > 0 {
                    match ops::prune(&main) {
                        Ok(_) => done.push(format!("{} 条失效记录", cleanup.stale)),
                        Err(err) => failed.push(format!("失效记录：{err}")),
                    }
                }
                let message = if done.is_empty() {
                    "什么都没清理掉".to_owned()
                } else {
                    format!("已清理 {}", done.join("、"))
                };
                if failed.is_empty() {
                    Ok(Report::Success(message))
                } else {
                    Ok(Report::Warning(format!(
                        "{message}\n没清理掉：\n{}",
                        failed.join("\n")
                    )))
                }
            },
            cx,
        );
    }

    /// Removes one merged worktree and its branch.
    fn confirm_clean(&mut self, worktree: Worktree, window: &mut Window, cx: &mut Context<Self>) {
        let description: SharedString = format!(
            "它的提交都已经在主分支里，也没有未提交的改动。删除这个工作区和分支 {} 不会丢任何东西{}。",
            worktree.label(),
            self.freed_text(&worktree.path)
        )
        .into();
        let title: SharedString = format!("清理“{}”？", worktree.name).into();
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, worktree) = (view.clone(), worktree.clone());
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("清理")
                .ok_variant(ButtonVariant::Primary)
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    let worktree = worktree.clone();
                    view.update(cx, |this, cx| this.clean(worktree, cx));
                    true
                })
        });
    }

    fn clean(&mut self, worktree: Worktree, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        self.run(
            Some((worktree.path.clone(), Busy::Clean)),
            "清理工作区",
            move || {
                ops::clean_merged(&main, &worktree.path, worktree.branch.as_deref())
                    .map(Report::Success)
            },
            cx,
        );
    }

    fn toggle_lock(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (Some(main), Some(worktree)) = (self.main_path(), self.worktree(&path).cloned()) else {
            return;
        };
        let lock = !worktree.locked;
        self.run(
            Some((path.clone(), Busy::Lock)),
            if lock { "锁定" } else { "解锁" },
            move || ops::set_locked(&main, &path, lock).map(Report::Success),
            cx,
        );
    }

    fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        let suggestion = suggest_path(&main, self.snapshot.as_ref());
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("../项目-feature")
                .default_value(suggestion)
        });
        let branch_input = cx.new(|cx| InputState::new(window, cx).placeholder("留空则用文件夹名"));
        let view = cx.entity();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                path_input.read(cx).focus_handle(cx).focus(window, cx);
            }
            let palette = Palette::current(cx);
            let create = {
                let (view, path_input, branch_input) =
                    (view.clone(), path_input.clone(), branch_input.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                    let path = path_input.read(cx).value().trim().to_owned();
                    let branch = branch_input.read(cx).value().trim().to_owned();
                    if path.is_empty() {
                        return;
                    }
                    view.update(cx, |this, cx| this.add(path, branch, cx));
                    window.close_dialog(cx);
                }
            };
            let field = |label: &'static str, hint: &'static str, input: Input| {
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(label),
                    )
                    .child(input)
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(palette.ink3)
                            .child(hint),
                    )
            };
            dialog
                .title("新建工作区")
                .width(px(480.))
                .child(
                    v_flex()
                        .pt(px(8.))
                        .gap(px(16.))
                        .child(field(
                            "位置",
                            "相对主工作区的路径，或者绝对路径。缓存会一起带过去，建好就能直接打开。",
                            Input::new(&path_input),
                        ))
                        .child(field(
                            "新分支",
                            "从主工作区当前的提交开出来。",
                            Input::new(&branch_input),
                        )),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("add-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("add-ok")
                                .primary()
                                .icon(IconName::Plus)
                                .label("创建")
                                .on_click(create),
                        ),
                )
        });
    }

    fn add(&mut self, path: String, branch: String, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        let watch = Arc::new(AddWatch::default());
        self.adding = Some((path.clone(), watch.clone()));
        self.start_ticker(cx);
        cx.notify();
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { ops::add(&main, &path, &branch, &watch) })
                .await;
            let note = match result {
                Ok(message) => Notification::success(message),
                Err(message) => Notification::error(message)
                    .title("新建工作区失败")
                    .autohide(false),
            };
            let _ = this.update(cx, |this, cx| {
                this.adding = None;
                this.refresh(Measure::Missing, cx);
            });
            let _ = window.update(cx, |_, window, cx| window.push_notification(note, cx));
        })
        .detach();
    }

    fn copy_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
        window.push_notification(Notification::info("路径已复制"), cx);
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        theme::toggle(window, cx);
        cx.notify();
    }
}

/// Measures the worktrees at `roots` together, leaving out of each the
/// worktrees nested inside it. `done` hears of each by its place in `roots`.
fn measure(
    roots: &[PathBuf],
    all: &[PathBuf],
    progress: &[Arc<ScanProgress>],
    reuse: bool,
    done: impl Fn(usize, Result<Usage, String>) + Sync,
) {
    // A memo holds while the worktrees are the ones it was made with: a
    // worktree made or removed adds or drops links to files in the others
    // without moving their directories' times.
    let mut worktrees: Vec<String> = all.iter().map(|path| path.display().to_string()).collect();
    worktrees.sort();
    let key = worktrees.join("\n");
    let files: Vec<Option<PathBuf>> = roots.iter().map(|root| state::memo_file(root)).collect();
    let memos: Vec<Option<usage::Memo>> = files
        .iter()
        .map(|file| {
            file.as_deref()
                .filter(|_| reuse)
                .and_then(|file| usage::Memo::load(file, &key))
        })
        .collect();
    let requests: Vec<ScanRequest<'_>> = roots
        .iter()
        .zip(progress)
        .zip(&memos)
        .map(|((root, progress), memo)| ScanRequest {
            root,
            skip: nested(root, all),
            tracked: usage::tracked_paths(root),
            links: true,
            previous: memo.as_ref(),
            progress,
        })
        .collect();
    usage::scan_all(&requests, |index, usage| {
        let usage = usage.map(|mut usage| {
            // A cancelled scan read only some directories: its memo would
            // replace a whole one with part of one.
            let memo = std::mem::take(&mut usage.memo);
            if !progress[index].cancel.load(Ordering::Relaxed)
                && let Some(file) = &files[index]
                && let Err(err) = memo.save(file, &key)
            {
                eprintln!("multibox-ui: cannot keep the measurement: {err}");
            }
            usage
        });
        done(index, usage.map_err(|err| err.to_string()));
    });
    state::forget_old_memos();
}

/// The worktrees inside `root`, relative to it with `/`.
fn nested(root: &Path, all: &[PathBuf]) -> HashSet<String> {
    all.iter()
        .filter(|other| other.as_path() != root)
        .filter_map(|other| other.strip_prefix(root).ok())
        .map(|rel| {
            rel.components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|rel| !rel.is_empty())
        .collect()
}

/// `../<repo>-2`, `../<repo>-3`, … whichever does not exist yet.
fn suggest_path(main: &Path, snapshot: Option<&Snapshot>) -> String {
    let name = main
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worktree".to_owned());
    let parent = main.parent().unwrap_or(main);
    let taken: HashSet<PathBuf> = snapshot
        .map(|snapshot| {
            snapshot
                .worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
                .collect()
        })
        .unwrap_or_default();
    (2..100)
        .map(|n| format!("{name}-{n}"))
        .find(|candidate| {
            let path = parent.join(candidate);
            !path.exists() && !taken.contains(&path)
        })
        .map(|candidate| format!("../{candidate}"))
        .unwrap_or_else(|| format!("../{name}-new"))
}
/// Opens a terminal in `path`, in whichever way this platform has.
fn open_terminal(path: &Path) {
    use std::process::Command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        // Windows Terminal splits its command line at every `;`, quoted or
        // not, and runs what follows as another command. So the path is never
        // put on it: the terminal starts in its own working directory.
        let opened = Command::new("wt.exe")
            .args(["-d", "."])
            .current_dir(path)
            .spawn()
            .is_ok();
        if !opened {
            let _ = Command::new("cmd.exe")
                .current_dir(path)
                .creation_flags(CREATE_NEW_CONSOLE)
                .spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open")
            .arg("-a")
            .arg("Terminal")
            .arg(path)
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("x-terminal-emulator")
            .current_dir(path)
            .spawn();
    }
}
