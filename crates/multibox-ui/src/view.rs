//! The window's state and what its buttons do. Drawing it is in [`render`].

mod render;

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
use std::time::{Duration, SystemTime};

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
    /// (bytes once hard-linked data is counted once, bytes that sharing saves)
    dedup: (u64, u64),
    selected: Option<PathBuf>,
    busy: HashMap<PathBuf, Busy>,
    global_busy: Option<SharedString>,
    adding: Option<(String, Arc<AddWatch>)>,
    refreshed_at: Option<SystemTime>,
    generation: u64,
    ticking: bool,
    window: AnyWindowHandle,
    focus: FocusHandle,
    _appearance: Subscription,
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
            dedup: (0, 0),
            selected: None,
            busy: HashMap::new(),
            global_busy: None,
            adding: None,
            refreshed_at: None,
            generation: 0,
            ticking: false,
            window: window.window_handle(),
            focus,
            _appearance: appearance,
        };
        app.refresh(true, cx);
        app
    }

    // ---- loading -------------------------------------------------------

    /// Reloads the worktree list and git facts. `rescan` measures every
    /// worktree again; otherwise only worktrees not measured yet are.
    fn refresh(&mut self, rescan: bool, cx: &mut Context<Self>) {
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
        let measured: HashSet<PathBuf> = if rescan {
            HashSet::new()
        } else {
            self.usage.keys().cloned().collect()
        };

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

            // One worktree at a time: they usually share a disk.
            let all: Vec<PathBuf> = snapshot
                .worktrees
                .iter()
                .filter(|worktree| worktree.exists)
                .map(|worktree| worktree.path.clone())
                .collect();
            for root in all.iter().filter(|root| !measured.contains(*root)) {
                let progress = Arc::new(ScanProgress::default());
                let current = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        this.scanning.insert(root.clone(), progress.clone());
                        this.start_ticker(cx);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !current {
                    return;
                }
                let (target, others, watch) = (root.clone(), all.clone(), progress.clone());
                let result = cx
                    .background_spawn(async move { measure(&target, &others, &watch) })
                    .await;
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
            self.dedup = (0, 0);
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
        self.refresh(true, cx);
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
                this.refresh(false, cx);
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

    fn freed_text(&self, path: &Path) -> String {
        self.freed(path)
            .map(|bytes| format!("，释放 {}", format::bytes(bytes)))
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
        !worktree.is_main
            && worktree.exists
            && !worktree.locked
            && worktree.branch.is_some()
            && details.ahead == Some(0)
            && !details.dirty()
            && details.editor.is_none()
            && !details.merge_in_progress
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
                this.refresh(false, cx);
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

/// Measures one worktree, leaving out the worktrees nested inside it.
fn measure(root: &Path, all: &[PathBuf], progress: &ScanProgress) -> Result<Usage, String> {
    let skip = all
        .iter()
        .filter(|other| other.as_path() != root)
        .filter_map(|other| other.strip_prefix(root).ok())
        .map(|rel| {
            rel.components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|rel| !rel.is_empty())
        .collect();
    usage::scan(&ScanRequest {
        root,
        skip,
        tracked: usage::tracked_paths(root),
        links: true,
        progress,
    })
    .map_err(|err| err.to_string())
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
