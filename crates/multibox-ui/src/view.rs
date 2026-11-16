//! The window: a summary of the repository's worktrees and their disk use,
//! a card per worktree with its actions, and a panel with the details of the
//! selected one.

use crate::format;
use crate::model::{self, Details, Snapshot, Worktree};
use crate::ops::{self, AddWatch};
use crate::usage::{self, ScanProgress, ScanRequest, Split, Usage};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, Sizable, Theme, ThemeMode, WindowExt, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, AnyWindowHandle, AppContext, ClickEvent, ClipboardItem, Context, FocusHandle,
    Focusable, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PathPromptOptions,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder,
    px, relative, rgb,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

gpui_kit::actions!(multibox, [Refresh]);

/// Colours of the four kinds of space, the same in light and dark mode.
const TRACKED: u32 = 0x3b82f6;
const UNTRACKED: u32 = 0xf59e0b;
const LINKED: u32 = 0x10b981;
const GIT: u32 = 0xa855f7;
/// One colour per worktree in the distribution bar.
const WORKTREE_COLORS: [u32; 8] = [
    0x6366f1, 0x06b6d4, 0xf43f5e, 0x84cc16, 0xf97316, 0x14b8a6, 0xec4899, 0xeab308,
];

fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn worktree_color(position: usize) -> Hsla {
    color(WORKTREE_COLORS[position % WORKTREE_COLORS.len()])
}

/// Theme colours copied out once per frame, so building elements does not
/// keep the theme borrowed.
#[derive(Clone)]
struct Palette {
    background: Hsla,
    foreground: Hsla,
    muted: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
    primary: Hsla,
    danger: Hsla,
    secondary: Hsla,
    radius: gpui_kit::Pixels,
    mono: SharedString,
    dark: bool,
}

impl Palette {
    fn new(theme: &Theme) -> Palette {
        Palette {
            background: theme.background,
            foreground: theme.foreground,
            muted: theme.muted,
            muted_foreground: theme.muted_foreground,
            border: theme.border,
            primary: theme.primary,
            danger: theme.danger,
            secondary: theme.secondary,
            radius: theme.radius_lg,
            mono: theme.mono_font_family.clone(),
            dark: theme.mode.is_dark(),
        }
    }
}

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
}

impl MultiboxApp {
    pub fn new(repo: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
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

    fn size_text(&self, path: &Path) -> String {
        self.freed(path)
            .map(|bytes| format!("，可释放约 {}", format::bytes(bytes)))
            .unwrap_or_default()
    }

    fn confirm_remove(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree(&path).cloned() else {
            return;
        };
        let details = self.details.get(&path).cloned().unwrap_or_default();
        let force = details.dirty();
        let mut lines = vec![format!(
            "{}（{}{}）会从磁盘上删除，git 也会忘掉它。",
            worktree.path.display(),
            worktree.label(),
            self.size_text(&path)
        )];
        if force {
            lines.push(format!(
                "它还有 {} 个改动的文件和 {} 个未跟踪的文件，删掉后找不回来。",
                details.changed, details.untracked
            ));
        }
        if details.ahead.is_some_and(|ahead| ahead > 0) {
            lines.push(format!(
                "分支上有 {} 个提交还没合并；分支本身会保留。",
                details.ahead.unwrap_or(0)
            ));
        }
        let description: SharedString = lines.join("\n").into();
        let title: SharedString = format!("删除 worktree “{}”？", worktree.name).into();
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, path) = (view.clone(), path.clone());
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text(if force { "强制删除" } else { "删除" })
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
            "删除 worktree",
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
            "把 '{branch}' 的 {} 个提交合并进主工作区的 '{into}'，然后删除这个 worktree{} 和分支 '{branch}'。\n合并有冲突时会停下来，什么都不删。",
            details.ahead.unwrap_or(0),
            self.size_text(&path)
        )
        .into();
        let title: SharedString = format!("合并并清理 “{}”？", worktree.name).into();
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

    fn merged(&self) -> Vec<Worktree> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        snapshot
            .worktrees
            .iter()
            .filter(|worktree| self.cleanable(worktree))
            .cloned()
            .collect()
    }

    fn confirm_clean(
        &mut self,
        targets: Vec<Worktree>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() {
            return;
        }
        let names: Vec<String> = targets
            .iter()
            .map(|worktree| {
                format!(
                    "· {}（{}{}）",
                    worktree.name,
                    worktree.label(),
                    self.size_text(&worktree.path)
                )
            })
            .collect();
        let description: SharedString = if targets.len() == 1 {
            format!(
                "它的提交都已经在主分支里，也没有未提交的改动。删除它和它的分支不会丢任何东西：\n{}",
                names.join("\n")
            )
        } else {
            format!(
                "这些 worktree 的提交都已经在主分支里，也没有未提交的改动。删除它们和它们的分支不会丢任何东西：\n{}",
                names.join("\n")
            )
        }
        .into();
        let title: SharedString = if targets.len() == 1 {
            format!("清理已合并的 “{}”？", targets[0].name).into()
        } else {
            format!("清理 {} 个已合并的 worktree？", targets.len()).into()
        };
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, targets) = (view.clone(), targets.clone());
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("清理")
                .ok_variant(ButtonVariant::Primary)
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    let targets = targets.clone();
                    view.update(cx, |this, cx| this.clean(targets, cx));
                    true
                })
        });
    }

    fn clean(&mut self, targets: Vec<Worktree>, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        if let [single] = targets.as_slice() {
            let worktree = single.clone();
            self.run(
                Some((worktree.path.clone(), Busy::Clean)),
                "清理 worktree",
                move || {
                    ops::clean_merged(&main, &worktree.path, worktree.branch.as_deref())
                        .map(Report::Success)
                },
                cx,
            );
            return;
        }
        self.run(
            None,
            "清理已合并的 worktree",
            move || {
                let mut done = Vec::new();
                let mut failed = Vec::new();
                for worktree in &targets {
                    match ops::clean_merged(&main, &worktree.path, worktree.branch.as_deref()) {
                        Ok(_) => done.push(worktree.name.clone()),
                        Err(err) => failed.push(format!("{}: {err}", worktree.name)),
                    }
                }
                let message = format!("已清理 {} 个：{}", done.len(), done.join("、"));
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

    fn prune(&mut self, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        self.run(
            None,
            "清除失效记录",
            move || ops::prune(&main).map(Report::Success),
            cx,
        );
    }

    fn purge_trash(&mut self, cx: &mut Context<Self>) {
        let Some(main) = self.main_path() else {
            return;
        };
        self.run(
            None,
            "清空残留目录",
            move || ops::purge_all_trash(&main).map(Report::Success),
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
        let branch_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("留空则用文件夹名作为新分支名"));
        let view = cx.entity();
        let main_text: SharedString = main.display().to_string().into();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                path_input.read(cx).focus_handle(cx).focus(window, cx);
            }
            let theme = Palette::new(cx.theme());
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
            dialog
                .title("新建 worktree")
                .width(px(520.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "用 Multibox 从主工作区 {main_text} 克隆，引擎缓存一起带过去，建好就能直接打开。"
                                )),
                        )
                        .child(field("路径（相对主工作区，或绝对路径）", Input::new(&path_input)))
                        .child(field("新分支名（可选）", Input::new(&branch_input))),
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
                    .title("新建 worktree 失败")
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
        window.push_notification(Notification::info(format!("已复制 {}", path.display())), cx);
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, Some(window), cx);
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

fn field(label: &'static str, input: Input) -> gpui_kit::Div {
    v_flex()
        .gap_1()
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
        .child(input)
}

// ---- rendering ----------------------------------------------------------

/// A horizontal bar of coloured segments, each a share of `total`.
fn segments(
    id: impl Into<SharedString>,
    parts: &[(Hsla, u64)],
    total: u64,
    height: f32,
    track: Hsla,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let total = total.max(1) as f32;
    h_flex()
        .id(id.into())
        .w_full()
        .h(px(height))
        .rounded(px(height / 2.0))
        .overflow_hidden()
        .bg(track)
        .children(
            parts
                .iter()
                .filter(|(_, bytes)| *bytes > 0)
                .map(|(hue, bytes)| {
                    div()
                        .h_full()
                        .w(relative((*bytes as f32 / total).max(0.004)))
                        .bg(*hue)
                }),
        )
}

fn split_parts(split: &Split) -> [(Hsla, u64); 4] {
    [
        (color(TRACKED), split.tracked),
        (color(UNTRACKED), split.untracked),
        (color(LINKED), split.linked),
        (color(GIT), split.git),
    ]
}

fn legend_dot(hue: Hsla) -> gpui_kit::Div {
    div().size(px(8.)).rounded_full().bg(hue).flex_none()
}

fn legend_item(hue: Hsla, label: &str, value: String, palette: &Palette) -> gpui_kit::Div {
    h_flex()
        .gap_1p5()
        .items_center()
        .child(legend_dot(hue))
        .child(
            div()
                .text_xs()
                .text_color(palette.muted_foreground)
                .child(label.to_owned()),
        )
        .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(value))
}

fn card(palette: &Palette) -> gpui_kit::Div {
    v_flex()
        .rounded(palette.radius)
        .border_1()
        .border_color(palette.border)
        .bg(palette.background)
}

fn stat_card(
    palette: &Palette,
    icon: IconName,
    title: &str,
    value: impl Into<SharedString>,
    sub: impl IntoElement,
) -> gpui_kit::Div {
    card(palette)
        .flex_1()
        .min_w(px(200.))
        .p_4()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .text_color(palette.muted_foreground)
                .child(Icon::new(icon).small())
                .child(div().text_sm().child(title.to_owned())),
        )
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(value.into()),
        )
        .child(sub)
}

impl MultiboxApp {
    fn render_header(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let repo_line = match &self.snapshot {
            Some(snapshot) => format!(
                "{}  ·  主分支 {}",
                snapshot.main.display(),
                snapshot.into.as_deref().unwrap_or("(detached)")
            ),
            None => self
                .repo
                .as_ref()
                .map(|repo| repo.display().to_string())
                .unwrap_or_else(|| "还没有打开仓库".to_owned()),
        };
        let has_repo = self.snapshot.is_some();
        h_flex()
            .px_6()
            .py_4()
            .gap_4()
            .items_center()
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .size(px(40.))
                    .rounded(palette.radius)
                    .bg(palette.primary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::FolderGit2)
                            .text_color(palette.background)
                            .size(px(22.)),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Multibox 工作区"),
                            )
                            .child(
                                Tag::secondary()
                                    .small()
                                    .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.muted_foreground)
                            .font_family(palette.mono.clone())
                            .truncate()
                            .child(repo_line),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("open-repo")
                            .outline()
                            .icon(IconName::FolderOpen)
                            .label("打开仓库")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_repo(cx))),
                    )
                    .child(
                        Button::new("add")
                            .primary()
                            .icon(IconName::Plus)
                            .label("新建 worktree")
                            .disabled(!has_repo || self.adding.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("refresh")
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label("刷新")
                            .tooltip("重新读取列表并重新测量所有 worktree（Ctrl/Cmd+R）")
                            .loading(self.loading)
                            .disabled(self.repo.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(true, cx))),
                    )
                    .child(
                        Button::new("theme")
                            .ghost()
                            .icon(if palette.dark {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            })
                            .tooltip(if palette.dark {
                                "浅色模式"
                            } else {
                                "深色模式"
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_theme(window, cx)),
                            ),
                    ),
            )
    }

    fn render_summary(
        &self,
        snapshot: &Snapshot,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let linked = snapshot.worktrees.iter().filter(|w| !w.is_main).count();
        let locked = snapshot.worktrees.iter().filter(|w| w.locked).count();
        let measured: u64 = snapshot
            .worktrees
            .iter()
            .filter_map(|worktree| self.usage.get(&worktree.path))
            .map(|usage| usage.split.total())
            .sum();
        let (actual, saved) = self.dedup;
        let scanning: u64 = self
            .scanning
            .values()
            .map(|progress| progress.bytes.load(Ordering::Relaxed))
            .sum();
        let pending = snapshot
            .worktrees
            .iter()
            .filter(|w| w.exists && !self.usage.contains_key(&w.path))
            .count();
        let storage_sub: AnyElement = if pending > 0 && !self.scanning.is_empty() {
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(palette.muted_foreground)
                .child(Icon::new(IconName::LoaderCircle).xsmall())
                .child(format!(
                    "还有 {pending} 个在测量，已读 {}",
                    format::bytes(scanning)
                ))
                .into_any_element()
        } else if saved > 0 {
            div()
                .text_xs()
                .text_color(palette.muted_foreground)
                .child(format!(
                    "硬链接共享后实际约 {}，省下 {}",
                    format::bytes(actual),
                    format::bytes(saved)
                ))
                .into_any_element()
        } else {
            div()
                .text_xs()
                .text_color(palette.muted_foreground)
                .child("所有 worktree 的文件大小之和")
                .into_any_element()
        };

        let volume = snapshot.volume_of(&snapshot.main).cloned();
        let main_details = self.details.get(&snapshot.main);
        let disk = match &volume {
            Some(volume) => {
                let used = volume.total.saturating_sub(volume.free);
                let share = used as f32 * 100.0 / volume.total.max(1) as f32;
                let fs = main_details
                    .and_then(|d| d.filesystem.clone())
                    .unwrap_or_else(|| volume.filesystem.clone());
                let reflink = main_details.map_or(volume.reflink, |d| d.reflink);
                stat_card(
                    palette,
                    IconName::HardDrive,
                    &format!("磁盘 {}", volume.root.trim_end_matches(['\\', '/'])),
                    format!("剩余 {}", format::bytes(volume.free)),
                    v_flex()
                        .gap_2()
                        .child(
                            Progress::new("disk")
                                .value(share)
                                .when(share > 90.0, |p| p.color(palette.danger)),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .text_xs()
                                .text_color(palette.muted_foreground)
                                .child(format!(
                                    "共 {} · 已用 {:.0}%",
                                    format::bytes(volume.total),
                                    share
                                ))
                                .child(if reflink {
                                    Tag::success().xsmall().child(format!("{fs} · reflink"))
                                } else {
                                    Tag::secondary().xsmall().child(format!("{fs} · 复制模式"))
                                }),
                        ),
                )
            }
            None => stat_card(
                palette,
                IconName::HardDrive,
                "磁盘",
                "—",
                div()
                    .text_xs()
                    .text_color(palette.muted_foreground)
                    .child("没找到所在的卷"),
            ),
        };

        let merged = self.merged();
        let merged_bytes: u64 = merged
            .iter()
            .filter_map(|worktree| self.freed(&worktree.path))
            .sum();
        let trash_bytes = self.trash_size.unwrap_or(0);
        let prunable = snapshot.worktrees.iter().filter(|w| w.prunable).count();
        let busy = self.global_busy.is_some();
        let reclaim = stat_card(
            palette,
            IconName::BrushCleaning,
            "可回收",
            format::bytes(merged_bytes + trash_bytes),
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.muted_foreground)
                        .child(format!(
                            "已合并 {} 个 · 残留目录 {} 个 · 失效记录 {} 条",
                            merged.len(),
                            snapshot.trash.len(),
                            prunable
                        )),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Button::new("clean-merged")
                                .small()
                                .primary()
                                .icon(IconName::BrushCleaning)
                                .label(format!("清理已合并 ({})", merged.len()))
                                .disabled(merged.is_empty() || busy)
                                .tooltip(
                                    "删除提交都已在主分支里、也没有改动的 worktree 和它们的分支",
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let merged = this.merged();
                                    this.confirm_clean(merged, window, cx)
                                })),
                        )
                        .when(!snapshot.trash.is_empty(), |row| {
                            row.child(
                                Button::new("purge-trash")
                                    .small()
                                    .outline()
                                    .icon(IconName::Trash)
                                    .label("清空残留")
                                    .tooltip("删除中断的删除操作留下的 .multibox-trash-* 目录")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.purge_trash(cx))),
                            )
                        })
                        .when(prunable > 0, |row| {
                            row.child(
                                Button::new("prune")
                                    .small()
                                    .outline()
                                    .icon(IconName::Eraser)
                                    .label("清除失效记录")
                                    .tooltip("git worktree prune：忘掉目录已不存在的 worktree")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.prune(cx))),
                            )
                        }),
                ),
        );

        h_flex()
            .gap_4()
            .flex_wrap()
            .items_stretch()
            .child(stat_card(
                palette,
                IconName::GitBranch,
                "Worktree",
                snapshot.worktrees.len().to_string(),
                div()
                    .text_xs()
                    .text_color(palette.muted_foreground)
                    .child(format!("主工作区 1 · 链接 {linked} · 锁定 {locked}")),
            ))
            .child(stat_card(
                palette,
                IconName::Database,
                "占用（文件大小）",
                format::bytes(measured),
                storage_sub,
            ))
            .child(disk)
            .child(reclaim)
    }

    fn render_distribution(
        &self,
        snapshot: &Snapshot,
        palette: &Palette,
    ) -> impl IntoElement + use<> {
        let parts: Vec<(usize, &Worktree, u64)> = snapshot
            .worktrees
            .iter()
            .enumerate()
            .filter_map(|(position, worktree)| {
                self.usage
                    .get(&worktree.path)
                    .map(|usage| (position, worktree, usage.split.total()))
            })
            .collect();
        let total: u64 = parts.iter().map(|(_, _, bytes)| bytes).sum();
        let mut whole = Split::default();
        for worktree in &snapshot.worktrees {
            if let Some(usage) = self.usage.get(&worktree.path) {
                whole.tracked += usage.split.tracked;
                whole.untracked += usage.split.untracked;
                whole.linked += usage.split.linked;
                whole.git += usage.split.git;
            }
        }
        let segments_by_worktree: Vec<(Hsla, u64)> = parts
            .iter()
            .map(|(position, _, bytes)| (worktree_color(*position), *bytes))
            .collect();
        card(palette)
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("空间分布"),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .flex_wrap()
                            .child(legend_item(
                                color(TRACKED),
                                "源码",
                                format::bytes(whole.tracked),
                                palette,
                            ))
                            .child(legend_item(
                                color(UNTRACKED),
                                "缓存/未跟踪",
                                format::bytes(whole.untracked),
                                palette,
                            ))
                            .child(legend_item(
                                color(LINKED),
                                "硬链接共享",
                                format::bytes(whole.linked),
                                palette,
                            ))
                            .child(legend_item(
                                color(GIT),
                                "Git 数据",
                                format::bytes(whole.git),
                                palette,
                            )),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted_foreground)
                    .child("按 worktree"),
            )
            .child(segments(
                "dist-worktrees",
                &segments_by_worktree,
                total,
                14.0,
                palette.muted,
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted_foreground)
                    .child("按类型"),
            )
            .child(segments(
                "dist-kinds",
                &split_parts(&whole),
                whole.total(),
                8.0,
                palette.muted,
            ))
            .child(h_flex().gap_4().flex_wrap().children(parts.iter().map(
                |(position, worktree, bytes)| {
                    let share = *bytes as f64 * 100.0 / total.max(1) as f64;
                    legend_item(
                        worktree_color(*position),
                        &worktree.name,
                        format!("{} · {share:.0}%", format::bytes(*bytes)),
                        palette,
                    )
                },
            )))
    }

    fn render_worktree(
        &self,
        position: usize,
        worktree: &Worktree,
        snapshot: &Snapshot,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let path = worktree.path.clone();
        let details = self.details.get(&path);
        let usage = self.usage.get(&path);
        let scanning = self.scanning.get(&path);
        let busy = self.busy.get(&path).copied();
        let selected = self.selected.as_ref() == Some(&path);
        let dirty = details.is_some_and(Details::dirty);
        let editor = details.and_then(|d| d.editor.clone());
        let ahead = details.and_then(|d| d.ahead);
        let behind = details.and_then(|d| d.behind);
        let merged = !worktree.is_main && worktree.branch.is_some() && ahead == Some(0);
        let id = |name: &str| SharedString::from(format!("{name}-{position}"));

        // Tags after the name.
        let mut tags: Vec<AnyElement> = Vec::new();
        tags.push(
            Tag::secondary()
                .small()
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(Icon::new(IconName::GitBranch).xsmall())
                        .child(worktree.label()),
                )
                .into_any_element(),
        );
        if worktree.is_main {
            tags.push(Tag::primary().small().child("主工作区").into_any_element());
        }
        if worktree.locked {
            tags.push(
                Tag::warning()
                    .outline()
                    .small()
                    .child("已锁定")
                    .into_any_element(),
            );
        }
        if worktree.prunable || !worktree.exists {
            tags.push(
                Tag::danger()
                    .outline()
                    .small()
                    .child("目录已不存在")
                    .into_any_element(),
            );
        }
        if let Some(details) = details {
            if details.changed > 0 {
                tags.push(
                    Tag::warning()
                        .outline()
                        .small()
                        .child(format!("{} 处改动", details.changed))
                        .into_any_element(),
                );
            }
            if details.untracked > 0 {
                tags.push(
                    Tag::secondary()
                        .outline()
                        .small()
                        .child(format!("{} 个未跟踪", details.untracked))
                        .into_any_element(),
                );
            }
            if details.merge_in_progress {
                tags.push(
                    Tag::danger()
                        .outline()
                        .small()
                        .child("合并进行中")
                        .into_any_element(),
                );
            }
            if self.cleanable(worktree) {
                tags.push(
                    Tag::success()
                        .outline()
                        .small()
                        .child("已合并，可清理")
                        .into_any_element(),
                );
            } else if merged && !dirty {
                tags.push(
                    Tag::secondary()
                        .outline()
                        .small()
                        .child("已合并")
                        .into_any_element(),
                );
            }
        }
        if let Some(editor) = &editor {
            tags.push(
                Tag::info()
                    .outline()
                    .small()
                    .child(format!("{editor} 打开中"))
                    .into_any_element(),
            );
        }

        // The size line.
        let size_block: AnyElement = match (usage, scanning) {
            (_, Some(progress)) if usage.is_none() => h_flex()
                .gap_2()
                .items_center()
                .text_xs()
                .text_color(palette.muted_foreground)
                .child(Icon::new(IconName::LoaderCircle).xsmall())
                .child(format!(
                    "测量中… {} 个文件 · {}",
                    format::count(progress.files.load(Ordering::Relaxed)),
                    format::bytes(progress.bytes.load(Ordering::Relaxed))
                ))
                .into_any_element(),
            (Some(usage), _) => {
                let split = usage.split;
                v_flex()
                    .gap_1p5()
                    .child(segments(
                        id("bar"),
                        &split_parts(&split),
                        split.total(),
                        8.0,
                        palette.muted,
                    ))
                    .child(
                        h_flex()
                            .gap_3()
                            .flex_wrap()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format::bytes(split.total())),
                            )
                            .child(legend_item(
                                color(TRACKED),
                                "源码",
                                format::bytes(split.tracked),
                                palette,
                            ))
                            .child(legend_item(
                                color(UNTRACKED),
                                "缓存",
                                format::bytes(split.untracked),
                                palette,
                            ))
                            .when(split.linked > 0, |row| {
                                row.child(legend_item(
                                    color(LINKED),
                                    "共享",
                                    format::bytes(split.linked),
                                    palette,
                                ))
                            })
                            .when(split.git >= 64 * 1024, |row| {
                                row.child(legend_item(
                                    color(GIT),
                                    "Git",
                                    format::bytes(split.git),
                                    palette,
                                ))
                            })
                            .when(scanning.is_some(), |row| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(palette.muted_foreground)
                                        .child("· 重新测量中"),
                                )
                            }),
                    )
                    .into_any_element()
            }
            _ => match self.scan_errors.get(&path) {
                Some(err) => div()
                    .text_xs()
                    .text_color(palette.danger)
                    .child(format!("测量失败：{err}"))
                    .into_any_element(),
                None if worktree.exists => div()
                    .text_xs()
                    .text_color(palette.muted_foreground)
                    .child("等待测量…")
                    .into_any_element(),
                None => div().into_any_element(),
            },
        };

        // Facts under the size.
        let mut facts: Vec<String> = Vec::new();
        if let (Some(ahead), Some(behind)) = (ahead, behind) {
            let into = snapshot.into.as_deref().unwrap_or("主分支");
            facts.push(format!("比 {into} 领先 {ahead} · 落后 {behind}"));
        }
        if let Some(commit) = details.and_then(|d| d.last_commit.as_ref()) {
            facts.push(format!("{} · {}", format::ago(commit.time), commit.subject));
        }
        if let Some(details) = details
            && !details.engines.is_empty()
        {
            facts.push(details.engines.join(" / "));
        }

        // Actions.
        let any_busy = busy.is_some() || self.global_busy.is_some();
        let removable = !worktree.is_main && worktree.exists;
        let blocked: Option<String> = if let Some(editor) = &editor {
            Some(format!("{editor} 正在使用，先关掉它"))
        } else if worktree.locked {
            Some("已锁定，先解锁".to_owned())
        } else {
            None
        };
        let mut actions: Vec<AnyElement> = Vec::new();
        if worktree.exists {
            let open = path.clone();
            actions.push(
                Button::new(id("open"))
                    .small()
                    .ghost()
                    .icon(IconName::FolderOpen)
                    .tooltip("在文件管理器中打开")
                    .on_click(move |_, _, cx| cx.open_with_system(&open))
                    .into_any_element(),
            );
            let term = path.clone();
            actions.push(
                Button::new(id("term"))
                    .small()
                    .ghost()
                    .icon(IconName::SquareTerminal)
                    .tooltip("在终端中打开")
                    .on_click(move |_, _, _| open_terminal(&term))
                    .into_any_element(),
            );
        }
        let copy = path.clone();
        actions.push(
            Button::new(id("copy"))
                .small()
                .ghost()
                .icon(IconName::Copy)
                .tooltip("复制路径")
                .on_click(cx.listener(move |this, _, window, cx| this.copy_path(&copy, window, cx)))
                .into_any_element(),
        );
        if removable {
            let target = path.clone();
            actions.push(
                Button::new(id("lock"))
                    .small()
                    .ghost()
                    .icon(if worktree.locked {
                        IconName::LockOpen
                    } else {
                        IconName::Lock
                    })
                    .tooltip(if worktree.locked {
                        "解锁"
                    } else {
                        "锁定：防止被删除或 prune"
                    })
                    .loading(busy == Some(Busy::Lock))
                    .disabled(any_busy)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_lock(target.clone(), cx)),
                    )
                    .into_any_element(),
            );
            if merged && !dirty {
                let target = worktree.clone();
                actions.push(
                    Button::new(id("clean"))
                        .small()
                        .success()
                        .icon(IconName::BrushCleaning)
                        .label("清理")
                        .tooltip("提交都已在主分支里：删除 worktree 和分支")
                        .loading(busy == Some(Busy::Clean))
                        .disabled(any_busy || blocked.is_some())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_clean(vec![target.clone()], window, cx)
                        }))
                        .into_any_element(),
                );
            } else if worktree.branch.is_some() && ahead.is_some_and(|ahead| ahead > 0) {
                let target = path.clone();
                let tooltip = if dirty {
                    "先提交改动".to_owned()
                } else {
                    blocked.clone().unwrap_or_else(|| {
                        "git multibox finish：合并进主分支，再删除 worktree 和分支".to_owned()
                    })
                };
                actions.push(
                    Button::new(id("finish"))
                        .small()
                        .outline()
                        .icon(IconName::GitMerge)
                        .label("合并并清理")
                        .tooltip(tooltip)
                        .loading(busy == Some(Busy::Finish))
                        .disabled(any_busy || dirty || blocked.is_some())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_finish(target.clone(), window, cx)
                        }))
                        .into_any_element(),
                );
            }
            let target = path.clone();
            actions.push(
                Button::new(id("remove"))
                    .small()
                    .danger()
                    .icon(IconName::Trash)
                    .label("删除")
                    .tooltip(
                        blocked
                            .clone()
                            .unwrap_or_else(|| "git multibox remove".to_owned()),
                    )
                    .loading(busy == Some(Busy::Remove))
                    .disabled(any_busy || blocked.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_remove(target.clone(), window, cx)
                    }))
                    .into_any_element(),
            );
        }

        let select = path.clone();
        card(palette)
            .id(id("card"))
            .p_4()
            .gap_2()
            .cursor_pointer()
            .when(selected, |card| card.border_color(palette.primary))
            .hover(|style| style.bg(palette.secondary))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = Some(select.clone());
                cx.notify();
            }))
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(
                        div()
                            .mt(px(6.))
                            .size(px(10.))
                            .rounded_full()
                            .flex_none()
                            .bg(worktree_color(position)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_wrap()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(worktree.name.clone()),
                                    )
                                    .children(tags),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.muted_foreground)
                                    .font_family(palette.mono.clone())
                                    .truncate()
                                    .child(worktree.path.display().to_string()),
                            ),
                    )
                    .child(h_flex().gap_1().flex_none().children(actions)),
            )
            .child(div().pl(px(22.)).child(size_block))
            .when(!facts.is_empty(), |card| {
                card.child(
                    div()
                        .pl(px(22.))
                        .text_xs()
                        .text_color(palette.muted_foreground)
                        .truncate()
                        .child(facts.join("   ·   ")),
                )
            })
    }

    fn render_detail(&self, snapshot: &Snapshot, palette: &Palette) -> AnyElement {
        let Some(worktree) = self
            .selected
            .as_ref()
            .and_then(|path| snapshot.worktrees.iter().find(|w| &w.path == path))
        else {
            return div().into_any_element();
        };
        let details = self.details.get(&worktree.path);
        let usage = self.usage.get(&worktree.path);
        let section = |title: &str| {
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(palette.muted_foreground)
                .child(title.to_owned())
        };
        let row = |label: &str, value: String| {
            h_flex()
                .gap_3()
                .justify_between()
                .text_sm()
                .child(
                    div()
                        .text_color(palette.muted_foreground)
                        .flex_none()
                        .child(label.to_owned()),
                )
                .child(div().truncate().child(value))
        };

        let mut body = v_flex().gap_4().p_5();
        body = body.child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(worktree.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.muted_foreground)
                        .font_family(palette.mono.clone())
                        .child(worktree.path.display().to_string()),
                ),
        );

        if let Some(usage) = usage {
            let split = usage.split;
            let largest = usage
                .entries
                .first()
                .map_or(1, |entry| entry.split.total())
                .max(1);
            body = body
                .child(
                    v_flex()
                        .gap_2()
                        .child(section("占用"))
                        .child(
                            h_flex()
                                .items_end()
                                .gap_2()
                                .child(div().text_3xl().font_weight(FontWeight::BOLD).child(format::bytes(split.total())))
                                .child(
                                    div()
                                        .pb_1()
                                        .text_xs()
                                        .text_color(palette.muted_foreground)
                                        .child(format!(
                                            "{} 个文件 · {:.1}s 测完",
                                            format::count(usage.files),
                                            usage.elapsed.as_secs_f32()
                                        )),
                                ),
                        )
                        .child(segments("detail-bar", &split_parts(&split), split.total(), 10.0, palette.muted))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(legend_item(color(TRACKED), "源码（git 跟踪）", format::bytes(split.tracked), palette))
                                .child(legend_item(color(UNTRACKED), "缓存 / 未跟踪", format::bytes(split.untracked), palette))
                                .child(legend_item(color(LINKED), "硬链接共享", format::bytes(split.linked), palette))
                                .when(split.git >= 64 * 1024, |list| {
                                    list.child(legend_item(color(GIT), "Git 数据（所有 worktree 共用）", format::bytes(split.git), palette))
                                }),
                        )
                        .when(details.is_some_and(|d| d.reflink), |block| {
                            block.child(
                                div()
                                    .text_xs()
                                    .text_color(palette.muted_foreground)
                                    .child("这个卷支持 reflink：没改过的文件和源 worktree 共享数据块，实际占用远小于这里的文件大小。"),
                            )
                        }),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .child(section("最大的目录"))
                        .children(usage.entries.iter().take(10).enumerate().map(|(i, entry)| {
                            let total = entry.split.total();
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .text_sm()
                                        .child(
                                            h_flex()
                                                .gap_1p5()
                                                .items_center()
                                                .min_w_0()
                                                .child(
                                                    Icon::new(if entry.dir {
                                                        IconName::Folder
                                                    } else {
                                                        IconName::File
                                                    })
                                                    .xsmall()
                                                    .text_color(palette.muted_foreground),
                                                )
                                                .child(div().truncate().child(entry.name.clone())),
                                        )
                                        .child(
                                            h_flex()
                                                .flex_none()
                                                .gap_2()
                                                .items_baseline()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(palette.muted_foreground)
                                                        .child(format!("{} 个文件", format::count(entry.files))),
                                                )
                                                .child(div().font_weight(FontWeight::MEDIUM).child(format::bytes(total))),
                                        ),
                                )
                                .child(
                                    div().w(relative(total as f32 / largest as f32)).child(segments(
                                        SharedString::from(format!("entry-{i}")),
                                        &split_parts(&entry.split),
                                        total,
                                        5.0,
                                        palette.muted,
                                    )),
                                )
                                .children(entry.children.iter().take(3).filter(|(_, bytes)| *bytes * 20 >= total).map(|(name, bytes)| {
                                    h_flex()
                                        .pl_4()
                                        .justify_between()
                                        .text_xs()
                                        .text_color(palette.muted_foreground)
                                        .child(div().truncate().child(format!("└ {name}")))
                                        .child(format::bytes(*bytes))
                                }))
                        })),
                );
        } else if let Some(progress) = self.scanning.get(&worktree.path) {
            body =
                body.child(
                    v_flex()
                        .gap_2()
                        .child(section("占用"))
                        .child(Progress::new("detail-scan").loading(true))
                        .child(div().text_xs().text_color(palette.muted_foreground).child(
                            format!(
                                "已读 {} 个文件，{}",
                                format::count(progress.files.load(Ordering::Relaxed)),
                                format::bytes(progress.bytes.load(Ordering::Relaxed))
                            ),
                        )),
                );
        }

        let mut git = v_flex().gap_2().child(section("Git"));
        git = git.child(row("分支", worktree.label()));
        if let Some(head) = &worktree.head {
            git = git.child(row("HEAD", model::short_hash(head).to_owned()));
        }
        if let Some(details) = details {
            if let (Some(ahead), Some(behind)) = (details.ahead, details.behind) {
                git = git.child(row(
                    &format!("对比 {}", snapshot.into.as_deref().unwrap_or("主分支")),
                    format!("领先 {ahead} · 落后 {behind}"),
                ));
            }
            git = git.child(row(
                "工作区",
                if details.dirty() {
                    format!(
                        "{} 处改动 · {} 个未跟踪",
                        details.changed, details.untracked
                    )
                } else {
                    "干净".to_owned()
                },
            ));
            if let Some(commit) = &details.last_commit {
                git = git.child(row("最近提交", format::ago(commit.time)));
                git = git.child(
                    div()
                        .text_xs()
                        .text_color(palette.muted_foreground)
                        .child(commit.subject.clone()),
                );
            }
        }
        body = body.child(git);

        if let Some(details) = details {
            let mut environment = v_flex().gap_2().child(section("环境"));
            if let Some(fs) = &details.filesystem {
                environment = environment.child(row(
                    "文件系统",
                    format!("{fs}{}", if details.reflink { " · reflink" } else { "" }),
                ));
            }
            environment = environment.child(row(
                "工程",
                if details.engines.is_empty() {
                    "没有识别到".to_owned()
                } else {
                    details.engines.join(", ")
                },
            ));
            if let Some(editor) = &details.editor {
                environment = environment.child(row("编辑器", editor.clone()));
            }
            for problem in &details.problems {
                environment = environment.child(
                    div()
                        .text_xs()
                        .text_color(palette.danger)
                        .child(problem.clone()),
                );
            }
            body = body.child(environment);
        }

        div()
            .id("detail")
            .size_full()
            .overflow_y_scrollbar()
            .child(body)
            .into_any_element()
    }

    fn render_adding(&self, palette: &Palette) -> Option<impl IntoElement + use<>> {
        let (path, watch) = self.adding.as_ref()?;
        let clone = watch.clone.lock().unwrap().clone();
        let note = watch.notes.lock().unwrap().last().cloned();
        let (value, text) = match &clone {
            Some((files, bytes, progress)) => {
                let done = progress.files.load(Ordering::Relaxed);
                let copied = progress.bytes.load(Ordering::Relaxed);
                (
                    Some(done as f32 * 100.0 / (*files).max(1) as f32),
                    format!(
                        "{} / {} 个文件 · {} / {}",
                        format::count(done),
                        format::count(*files),
                        format::bytes(copied),
                        format::bytes(*bytes)
                    ),
                )
            }
            None => (None, "准备中…".to_owned()),
        };
        Some(
            card(palette)
                .p_4()
                .gap_2()
                .border_color(palette.primary)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Icon::new(IconName::LoaderCircle).small())
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("正在创建 {path}")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(palette.muted_foreground)
                                .child(text),
                        ),
                )
                .child(match value {
                    Some(value) => Progress::new("adding").value(value),
                    None => Progress::new("adding").loading(true),
                })
                .when_some(note, |card, note| {
                    card.child(
                        div()
                            .text_xs()
                            .text_color(palette.muted_foreground)
                            .child(note),
                    )
                }),
        )
    }

    fn render_empty(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (title, detail) = match &self.error {
            Some(err) => ("打不开这个仓库".to_owned(), err.clone()),
            None if self.repo.is_some() => ("正在读取…".to_owned(), String::new()),
            None => (
                "打开一个 git 仓库".to_owned(),
                "选择仓库里的任意目录，就能看到它的所有 worktree、各自占用的空间，并在这里清理。"
                    .to_owned(),
            ),
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::FolderGit2)
                    .size(px(48.))
                    .text_color(palette.muted_foreground),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .max_w(px(520.))
                    .text_sm()
                    .text_center()
                    .text_color(palette.muted_foreground)
                    .child(detail),
            )
            .child(
                Button::new("empty-open")
                    .primary()
                    .icon(IconName::FolderOpen)
                    .label("打开仓库")
                    .on_click(cx.listener(|this, _, _, cx| this.pick_repo(cx))),
            )
    }

    fn render_status(&self, palette: &Palette) -> impl IntoElement + use<> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(at) = self.refreshed_at {
            parts.push(format!("刷新于 {}", format::clock(at)));
        }
        if let Some(snapshot) = &self.snapshot {
            let measured = snapshot
                .worktrees
                .iter()
                .filter(|w| self.usage.contains_key(&w.path))
                .count();
            parts.push(format!("已测量 {measured}/{}", snapshot.worktrees.len()));
        }
        if let Some(busy) = &self.global_busy {
            parts.push(format!("{busy}…"));
        }
        h_flex()
            .px_6()
            .py_2()
            .gap_4()
            .justify_between()
            .border_t_1()
            .border_color(palette.border)
            .text_xs()
            .text_color(palette.muted_foreground)
            .child(parts.join("   ·   "))
            .child("大小是文件大小之和；NTFS 上的硬链接按文件 ID 去重后计入“实际”。")
    }
}

impl Render for MultiboxApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::new(cx.theme());
        let header = self.render_header(&palette, cx);
        let status = self.render_status(&palette);

        let content: AnyElement = match self.snapshot.clone() {
            None => self.render_empty(&palette, cx).into_any_element(),
            Some(snapshot) => {
                let cards: Vec<AnyElement> = snapshot
                    .worktrees
                    .iter()
                    .enumerate()
                    .map(|(position, worktree)| {
                        self.render_worktree(position, worktree, &snapshot, &palette, cx)
                            .into_any_element()
                    })
                    .collect();
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        div()
                            .id("main")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scrollbar()
                            .child(
                                v_flex()
                                    .p_6()
                                    .gap_4()
                                    .child(self.render_summary(&snapshot, &palette, cx))
                                    .children(self.render_adding(&palette))
                                    .child(self.render_distribution(&snapshot, &palette))
                                    .child(h_flex().justify_between().child(
                                        div().text_sm().font_weight(FontWeight::SEMIBOLD).child(
                                            format!(
                                                "全部 worktree（{}）",
                                                snapshot.worktrees.len()
                                            ),
                                        ),
                                    ))
                                    .children(cards),
                            ),
                    )
                    .child(
                        div()
                            .w(px(380.))
                            .flex_none()
                            .h_full()
                            .border_l_1()
                            .border_color(palette.border)
                            .child(self.render_detail(&snapshot, &palette)),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(true, cx)))
            .bg(palette.background)
            .text_color(palette.foreground)
            .child(header)
            .child(content)
            .child(status)
    }
}

/// Opens a terminal in `path`, in whichever way this platform has.
fn open_terminal(path: &Path) {
    use std::process::Command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        let opened = Command::new("wt.exe").arg("-d").arg(path).spawn().is_ok();
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
