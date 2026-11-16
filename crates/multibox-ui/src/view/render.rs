//! Drawing the window.
//!
//! Three parts, each answering one question. The overview: how much disk do
//! the worktrees take, and what can go? The list: which worktrees are there,
//! how big is each and what state is it in? The inspector: everything about
//! the selected one, and what can be done with it.
//!
//! Rows carry no buttons. A worktree is acted on from the inspector, where
//! each action has room for its name.

use super::{Busy, Cleanup, MultiboxApp, Refresh, open_terminal};
use crate::format;
use crate::model::{self, Details, Snapshot, Worktree};
use crate::theme::Palette;
use crate::usage::Split;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable, Icon, Sizable, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AnyView, App, ClickEvent, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, relative,
};
use std::sync::atomic::Ordering;

/// Below this, the git share of a worktree is its one-line `.git` file.
const GIT_WORTH_SHOWING: u64 = 64 * 1024;

/// The narrowest part of a bar worth drawing, in pixels.
const NARROWEST: f32 = 3.;

/// About how long the bars run at full length, in pixels: the overview's, a
/// row's at the size of the largest worktree, and the inspector's.
const TILE_SPAN: f32 = 240.;
const ROW_SPAN: f32 = 300.;
const PANEL_SPAN: f32 = 300.;
const ENTRY_SPAN: f32 = 96.;

fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text: SharedString = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

fn glyph(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).size(px(size)).text_color(color)
}

fn text(size: f32, color: Hsla) -> Div {
    div().text_size(px(size)).text_color(color)
}

/// Text of `size` points that sits on the same baseline as `largest`-point
/// text beside it, in a row that aligns its children's bottoms
/// (`items_end`).
///
/// GPUI aligns boxes, not baselines: asked for baseline alignment it lines up
/// their bottoms, and a small label, whose line box has more room around the
/// glyphs, then floats above a large number. So every piece is set exactly one
/// line high, and the smaller ones are lifted by the difference between where
/// the baselines fall in boxes of the two sizes.
fn run(size: f32, largest: f32, color: Hsla, palette: &Palette) -> Div {
    text(size, color)
        .line_height(px(size))
        .pb(px((largest - size) * palette.baseline))
}

/// A size with its number set large and its unit small beside it.
fn figure(bytes: u64, number: f32, unit: f32, palette: &Palette) -> Div {
    let (value, name) = format::size(bytes);
    h_flex()
        .items_end()
        .gap(px(unit * 0.3))
        .child(
            run(number, number, palette.ink, palette)
                .font_weight(FontWeight::SEMIBOLD)
                .child(value),
        )
        .child(run(unit, number, palette.ink2, palette).child(name))
}

fn caption(label: impl Into<SharedString>, palette: &Palette) -> Div {
    text(12., palette.ink3)
        .font_weight(FontWeight::MEDIUM)
        .child(label.into())
}

/// A card: white on the grey ground, graphite on the dark one.
fn card(palette: &Palette) -> Div {
    v_flex()
        .bg(palette.card)
        .border_1()
        .border_color(palette.hairline)
        .rounded(px(10.))
}

/// The overview's figures, and the bars under them.
const FIGURE: f32 = 26.;
const UNIT: f32 = 13.;
const BAR: f32 = 8.;

/// One line of quiet text that never wraps.
fn line(words: impl Into<SharedString>, palette: &Palette) -> Div {
    text(12., palette.ink3).truncate().child(words.into())
}

/// One figure of the overview. Every tile has the same four lines, each of a
/// fixed height: a title with its icon, the figure, a sentence, and a bar or
/// a button. Side by side, each line then runs straight across all of them.
fn tile(
    icon: IconName,
    title: impl Into<SharedString>,
    meta: Option<AnyElement>,
    figure: impl IntoElement,
    sentence: impl IntoElement,
    foot: Option<AnyElement>,
    palette: &Palette,
) -> Div {
    card(palette)
        .flex_1()
        .min_w(px(160.))
        .px(px(20.))
        .py(px(16.))
        .child(
            h_flex()
                .h(px(18.))
                .gap(px(8.))
                .child(glyph(icon, 14., palette.ink3).flex_none())
                .child(caption(title, palette).flex_1().min_w_0().truncate())
                .children(meta),
        )
        .child(h_flex().mt(px(14.)).h(px(FIGURE)).items_end().child(figure))
        .child(
            h_flex()
                .mt(px(8.))
                .h(px(18.))
                .min_w_0()
                .overflow_hidden()
                .child(sentence),
        )
        .child(
            h_flex()
                .mt(px(12.))
                .h(px(26.))
                .w_full()
                .children(foot.map(|foot| h_flex().w_full().child(foot))),
        )
}

/// One kind of space in a bar.
struct Part {
    label: &'static str,
    long: &'static str,
    color: Hsla,
    /// Shared data: drawn hollow, since it takes no room of its own.
    ghost: bool,
    bytes: u64,
}

fn parts(split: &Split, palette: &Palette) -> [Part; 4] {
    [
        Part {
            label: "源码",
            long: "源码",
            color: palette.source,
            ghost: false,
            bytes: split.tracked,
        },
        Part {
            label: "缓存",
            long: "缓存和未跟踪的文件",
            color: palette.cache,
            ghost: false,
            bytes: split.untracked,
        },
        Part {
            label: "共享",
            long: "与其他工作区共享",
            color: palette.shared,
            ghost: true,
            bytes: split.linked,
        },
        Part {
            label: "Git",
            long: "Git 数据",
            color: palette.git,
            ghost: false,
            bytes: split.git,
        },
    ]
}

fn swatch(part: &Part, palette: &Palette) -> Div {
    div()
        .size(px(8.))
        .flex_none()
        .rounded(px(2.))
        .bg(part.color)
        .when(part.ghost, |mark| {
            mark.border_1().border_color(palette.cache)
        })
}

/// A bar split by kind, with a gap between neighbours and rounded ends. Each
/// part answers a hover with its name, size and share. `span` is about how
/// many pixels the bar runs.
fn stack(
    id: impl Into<ElementId>,
    parts: &[Part],
    height: f32,
    span: f32,
    palette: &Palette,
) -> Stateful<Div> {
    let total: u64 = parts.iter().map(|part| part.bytes).sum();
    // A part that would come out under a few pixels wide is not drawn. Next to
    // the gap it reads as a stray line rather than a share, and stretched to
    // something one can see it would claim several times its size. Rows are
    // shortened to their worktree's size, so the test is in pixels, not in a
    // share of the bar. The size is still in the legend, the tooltip and the
    // inspector's table.
    let shown: Vec<(usize, &Part)> = parts
        .iter()
        .enumerate()
        .filter(|(_, part)| {
            part.bytes > 0 && part.bytes as f32 / total.max(1) as f32 * span >= NARROWEST
        })
        .collect();
    let last = shown.len().saturating_sub(1);
    h_flex()
        .id(id)
        .w_full()
        .h(px(height))
        .gap(px(2.))
        .children(shown.into_iter().enumerate().map(|(place, (index, part))| {
            div()
                .id(index)
                .h_full()
                .flex_grow(part.bytes as f32 / total.max(1) as f32)
                .flex_shrink(1.)
                .flex_basis(px(0.))
                .min_w(px(2.))
                .rounded(px(1.5))
                .when(place == 0, |bar| bar.rounded_l(px(4.)))
                .when(place == last, |bar| bar.rounded_r(px(4.)))
                .bg(part.color)
                .when(part.ghost, |bar| bar.border_1().border_color(palette.cache))
                .tooltip(tip(format!(
                    "{} {} · {}",
                    part.long,
                    format::bytes(part.bytes),
                    format::percent(part.bytes, total)
                )))
        }))
}

/// A state of a worktree, as an icon with a word or two.
struct Mark {
    icon: IconName,
    color: Hsla,
    label: String,
    hint: String,
}

fn mark(mark: Mark, id: usize, palette: &Palette) -> Stateful<Div> {
    h_flex()
        .id(id)
        .flex_none()
        .gap(px(4.))
        .child(glyph(mark.icon, 12., mark.color))
        .child(text(12., palette.ink2).child(mark.label))
        .tooltip(tip(mark.hint))
}

impl MultiboxApp {
    /// The states worth a glance, most pressing first.
    fn marks(
        &self,
        worktree: &Worktree,
        details: Option<&Details>,
        snapshot: &Snapshot,
        palette: &Palette,
    ) -> Vec<Mark> {
        let into = snapshot.into.as_deref().unwrap_or("主分支");
        let mut marks = Vec::new();
        if worktree.prunable || !worktree.exists {
            marks.push(Mark {
                icon: IconName::TriangleAlert,
                color: palette.danger,
                label: "目录不在了".to_owned(),
                hint: "这个工作区的目录已经不存在，清理时会移除这条记录".to_owned(),
            });
        }
        if worktree.is_main {
            marks.push(Mark {
                icon: IconName::House,
                color: palette.ink3,
                label: "主工作区".to_owned(),
                hint: "其他工作区的分支合并到这里".to_owned(),
            });
        }
        if worktree.locked {
            marks.push(Mark {
                icon: IconName::Lock,
                color: palette.warning,
                label: "已锁定".to_owned(),
                hint: "锁定的工作区不能删除，先解锁".to_owned(),
            });
        }
        let Some(details) = details else {
            return marks;
        };
        if let Some(editor) = &details.editor {
            marks.push(Mark {
                icon: IconName::AppWindow,
                color: palette.cache,
                label: "编辑器开着".to_owned(),
                hint: format!("{editor} 正在使用这个工作区"),
            });
        }
        if details.merge_in_progress {
            marks.push(Mark {
                icon: IconName::GitMerge,
                color: palette.danger,
                label: "合并没做完".to_owned(),
                hint: "有一次合并停在冲突上，解决后提交，或者 git merge --abort".to_owned(),
            });
        }
        if details.dirty() {
            marks.push(Mark {
                icon: IconName::Pencil,
                color: palette.warning,
                label: format!("{} 处未提交", details.changed + details.untracked),
                hint: format!(
                    "{} 个文件有改动，{} 个文件未跟踪",
                    details.changed, details.untracked
                ),
            });
        }
        if let Some(ahead) = details.ahead.filter(|ahead| *ahead > 0) {
            marks.push(Mark {
                icon: IconName::ArrowUp,
                color: palette.ink3,
                label: format!("领先 {ahead}"),
                hint: format!("有 {ahead} 个提交还没合并进 {into}"),
            });
        }
        if let Some(behind) = details.behind.filter(|behind| *behind > 0) {
            marks.push(Mark {
                icon: IconName::ArrowDown,
                color: palette.ink3,
                label: format!("落后 {behind}"),
                hint: format!("{into} 上有 {behind} 个提交这里还没有"),
            });
        }
        if self.cleanable(worktree) {
            marks.push(Mark {
                icon: IconName::CircleCheck,
                color: palette.success,
                label: "已合并，可清理".to_owned(),
                hint: format!("提交都已经在 {into} 里，也没有未提交的改动"),
            });
        }
        if let Some(commit) = &details.last_commit {
            marks.push(Mark {
                icon: IconName::Clock,
                color: palette.ink3,
                label: format::ago(commit.time),
                hint: format!("最近一次提交：{}", commit.subject),
            });
        }
        marks
    }

    // ---- top bar -------------------------------------------------------

    fn header(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let title = match &self.snapshot {
            Some(snapshot) => {
                let name = snapshot
                    .main
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| snapshot.main.display().to_string());
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(10.))
                    .child(
                        text(15., palette.ink)
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .h(px(22.))
                            .px(px(8.))
                            .gap(px(4.))
                            .rounded(px(6.))
                            .bg(palette.selected)
                            .child(glyph(IconName::GitBranch, 12., palette.ink3))
                            .child(
                                text(12., palette.ink2)
                                    .child(snapshot.into.clone().unwrap_or("detached".to_owned())),
                            ),
                    )
                    .child(
                        text(12., palette.ink3)
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(palette.mono.clone())
                            .child(snapshot.main.display().to_string()),
                    )
            }
            None => h_flex().flex_1().child(
                text(15., palette.ink)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Multibox"),
            ),
        };
        let refreshed = self
            .refreshed_at
            .map(|at| format!("刷新并重新测量（F5）· 上次 {}", format::clock(at)))
            .unwrap_or_else(|| "刷新并重新测量（F5）".to_owned());
        h_flex()
            .flex_none()
            .h(px(56.))
            .px(px(24.))
            .gap(px(16.))
            .bg(palette.card)
            .border_b_1()
            .border_color(palette.hairline)
            .child(title)
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(2.))
                    .child(
                        Button::new("open-repo")
                            .ghost()
                            .icon(IconName::FolderOpen)
                            .accessibility_label("打开其他仓库")
                            .tooltip("打开其他仓库")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_repo(cx))),
                    )
                    .child(
                        Button::new("refresh")
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .accessibility_label("刷新")
                            .tooltip(refreshed)
                            .loading(self.loading || !self.scanning.is_empty())
                            .disabled(self.repo.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(true, cx))),
                    )
                    .child(
                        Button::new("theme")
                            .ghost()
                            .accessibility_label("切换深色和浅色")
                            .icon(if palette.dark {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            })
                            .tooltip(if palette.dark {
                                "换成浅色"
                            } else {
                                "换成深色"
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_theme(window, cx)),
                            ),
                    ),
            )
            .child(
                Button::new("add")
                    .outline()
                    .icon(IconName::Plus)
                    .label("新建工作区")
                    .disabled(self.snapshot.is_none() || self.adding.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx))),
            )
            .into_any_element()
    }

    // ---- overview ------------------------------------------------------

    /// Every kind of space summed over the measured worktrees, as files count
    /// it and as the disk holds it, where data shared through hard links is
    /// there once.
    fn totals(&self, snapshot: &Snapshot) -> (Split, Split) {
        let mut whole = Split::default();
        for worktree in &snapshot.worktrees {
            if let Some(usage) = self.usage.get(&worktree.path) {
                whole.tracked += usage.split.tracked;
                whole.untracked += usage.split.untracked;
                whole.linked += usage.split.linked;
                whole.git += usage.split.git;
            }
        }
        let (_, saved) = self.dedup;
        let on_disk = Split {
            linked: whole.linked.saturating_sub(saved),
            ..whole
        };
        (whole, on_disk)
    }

    fn overview(
        &self,
        snapshot: &Snapshot,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reflink = self
            .details
            .get(&snapshot.main)
            .is_some_and(|details| details.reflink);
        h_flex()
            .items_stretch()
            .flex_wrap()
            .gap(px(16.))
            .child(self.occupied(snapshot, reflink, palette))
            .child(self.disk(snapshot, reflink, palette))
            .child(self.reclaim(palette, cx))
            .into_any_element()
    }

    fn occupied(&self, snapshot: &Snapshot, reflink: bool, palette: &Palette) -> Div {
        let present = snapshot.worktrees.iter().filter(|w| w.exists).count();
        let measured = snapshot
            .worktrees
            .iter()
            .filter(|w| self.usage.contains_key(&w.path))
            .count();
        let (whole, on_disk) = self.totals(snapshot);
        let (actual, saved) = self.dedup;

        let sentence: AnyElement = if measured < present {
            h_flex()
                .gap(px(6.))
                .child(Spinner::new().xsmall().color(palette.ink3))
                .child(line(
                    format!("正在测量，{measured}/{present} 个工作区"),
                    palette,
                ))
                .into_any_element()
        } else if saved > 0 {
            line(format!("共享省下 {}", format::bytes(saved)), palette).into_any_element()
        } else {
            line(format!("{present} 个工作区"), palette).into_any_element()
        };
        let explain = if reflink {
            format!(
                "各工作区的文件合计 {}。这个卷上没改过的文件和源工作区共用数据，实际占用比这里小。",
                format::bytes(whole.total())
            )
        } else {
            format!(
                "各工作区的文件合计 {}，通过硬链接共用的只算一次。",
                format::bytes(whole.total())
            )
        };
        let info = div()
            .id("usage-info")
            .child(glyph(IconName::Info, 13., palette.ink3))
            .tooltip(tip(explain))
            .into_any_element();
        tile(
            IconName::Layers,
            "占用磁盘",
            Some(info),
            figure(actual, FIGURE, UNIT, palette),
            sentence,
            Some(
                stack("whole", &parts(&on_disk, palette), BAR, TILE_SPAN, palette)
                    .into_any_element(),
            ),
            palette,
        )
    }

    fn disk(&self, snapshot: &Snapshot, reflink: bool, palette: &Palette) -> Div {
        let Some(volume) = snapshot.volume_of(&snapshot.main) else {
            return tile(
                IconName::HardDrive,
                "磁盘",
                None,
                run(FIGURE, FIGURE, palette.ink3, palette).child("—"),
                line("没找到所在的卷", palette),
                None,
                palette,
            );
        };
        let used = volume.total.saturating_sub(volume.free);
        let share = used as f32 / volume.total.max(1) as f32;
        let filesystem = self
            .details
            .get(&snapshot.main)
            .and_then(|details| details.filesystem.clone())
            .unwrap_or_else(|| volume.filesystem.clone());
        let mode = if reflink {
            "这个卷支持 reflink：新工作区和源工作区共用没改过的数据，几乎不占空间。"
        } else {
            "这个卷上新工作区是完整的一份，只有写一次就不再改的缓存通过硬链接共用。放到 Dev Drive 上可以共用全部数据。"
        };
        let tag = h_flex()
            .id("filesystem")
            .flex_none()
            .h(px(18.))
            .px(px(6.))
            .rounded(px(4.))
            .bg(palette.selected)
            .child(text(11., palette.ink2).child(filesystem))
            .tooltip(tip(mode))
            .into_any_element();
        let meter = div()
            .w_full()
            .h(px(BAR))
            .rounded(px(BAR / 2.))
            .bg(palette.selected)
            .child(
                div()
                    .h_full()
                    .w(relative(share.clamp(0.01, 1.)))
                    .rounded(px(BAR / 2.))
                    .bg(if share > 0.9 {
                        palette.danger
                    } else {
                        palette.ink3
                    }),
            )
            .into_any_element();
        tile(
            IconName::HardDrive,
            format!("磁盘 {}", volume.root.trim_end_matches(['\\', '/'])),
            Some(tag),
            h_flex()
                .items_end()
                .gap(px(6.))
                .child(figure(volume.free, FIGURE, UNIT, palette))
                .child(run(UNIT, FIGURE, palette.ink2, palette).child("可用")),
            line(
                format!(
                    "已用 {}，共 {}",
                    format::percent(used, volume.total),
                    format::bytes(volume.total)
                ),
                palette,
            ),
            Some(meter),
            palette,
        )
    }

    fn reclaim(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let cleanup: Cleanup = self.cleanup();
        if cleanup.is_empty() {
            return tile(
                IconName::BrushCleaning,
                "可清理",
                None,
                figure(0, FIGURE, UNIT, palette),
                h_flex()
                    .gap(px(6.))
                    .child(glyph(IconName::CircleCheck, 13., palette.success))
                    .child(line("没有要清理的", palette)),
                None,
                palette,
            );
        }
        let mut what = Vec::new();
        if !cleanup.merged.is_empty() {
            what.push(format!("{} 个已合并", cleanup.merged.len()));
        }
        if cleanup.trash > 0 {
            what.push(format!("{} 个残留目录", cleanup.trash));
        }
        if cleanup.stale > 0 {
            what.push(format!("{} 条失效记录", cleanup.stale));
        }
        tile(
            IconName::BrushCleaning,
            "可清理",
            None,
            figure(cleanup.bytes, FIGURE, UNIT, palette),
            line(what.join(" · "), palette),
            Some(
                Button::new("clean-up")
                    .small()
                    .outline()
                    .icon(IconName::BrushCleaning)
                    .label("清理")
                    .tooltip("先列出会清掉什么，确认后再动手")
                    .loading(self.global_busy.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_cleanup(window, cx)))
                    .into_any_element(),
            ),
            palette,
        )
    }

    /// The title of the list, with the key to every bar in it and above it.
    fn list_head(&self, snapshot: &Snapshot, palette: &Palette) -> Div {
        let (_, on_disk) = self.totals(snapshot);
        let kinds = parts(&on_disk, palette);
        h_flex()
            .flex_wrap()
            .gap_x(px(16.))
            .gap_y(px(6.))
            .px(px(14.))
            .pt(px(10.))
            .pb(px(8.))
            .child(
                h_flex()
                    .flex_1()
                    .gap(px(8.))
                    .child(caption("工作区", palette))
                    .child(text(12., palette.ink3).child(snapshot.worktrees.len().to_string())),
            )
            .child(h_flex().flex_wrap().gap_x(px(16.)).gap_y(px(4.)).children(
                kinds.iter().filter(|part| part.bytes > 0).map(|part| {
                    h_flex()
                        .gap(px(6.))
                        .child(swatch(part, palette))
                        .child(text(12., palette.ink2).child(part.label))
                        .child(
                            text(12., palette.ink)
                                .font_weight(FontWeight::MEDIUM)
                                .child(format::bytes(part.bytes)),
                        )
                }),
            ))
    }

    // ---- the list ------------------------------------------------------

    fn row(
        &self,
        index: usize,
        worktree: &Worktree,
        largest: u64,
        snapshot: &Snapshot,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = worktree.path.clone();
        let details = self.details.get(&path);
        let usage = self.usage.get(&path);
        let selected = self.selected.as_ref() == Some(&path);

        let marks = self.marks(worktree, details, snapshot, palette);
        let identity = v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(5.))
            .child(
                h_flex()
                    .gap(px(10.))
                    .child(
                        text(14., palette.ink)
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(worktree.name.clone()),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap(px(4.))
                            .child(glyph(IconName::GitBranch, 12., palette.ink3).flex_none())
                            .child(text(12., palette.ink2).truncate().child(worktree.label())),
                    ),
            )
            .child(
                h_flex().gap_x(px(14.)).overflow_hidden().children(
                    marks
                        .into_iter()
                        .enumerate()
                        .map(|(id, item)| mark(item, id, palette)),
                ),
            );

        let bar: AnyElement = match (usage, self.scanning.get(&path)) {
            (Some(usage), _) => {
                let total = usage.split.total();
                let length = (total as f32 / largest.max(1) as f32).clamp(0.02, 1.);
                div()
                    .w(relative(length))
                    .child(stack(
                        "bar",
                        &parts(&usage.split, palette),
                        8.,
                        ROW_SPAN * length,
                        palette,
                    ))
                    .into_any_element()
            }
            (None, Some(progress)) => h_flex()
                .gap(px(6.))
                .child(Spinner::new().xsmall().color(palette.ink3))
                .child(text(12., palette.ink3).child(format!(
                    "正在测量，{} 个文件",
                    format::count(progress.files.load(Ordering::Relaxed))
                )))
                .into_any_element(),
            (None, None) => match self.scan_errors.get(&path) {
                Some(error) => div()
                    .id("scan-error")
                    .child(text(12., palette.danger).child("量不了"))
                    .tooltip(tip(error.clone()))
                    .into_any_element(),
                None if worktree.exists => {
                    text(12., palette.ink3).child("等待测量").into_any_element()
                }
                None => div().into_any_element(),
            },
        };
        let size: AnyElement = match usage {
            Some(usage) => figure(usage.split.total(), 14., 12., palette).into_any_element(),
            None => text(14., palette.ink3).child("—").into_any_element(),
        };

        let select = path.clone();
        h_flex()
            .id(("row", index))
            .w_full()
            .gap(px(24.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(7.))
            .cursor_pointer()
            .map(|row| {
                if selected {
                    row.bg(palette.selected)
                } else {
                    row.hover(|style| style.bg(palette.hover))
                }
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = Some(select.clone());
                cx.notify();
            }))
            .child(identity)
            .child(div().flex_none().w(relative(0.36)).child(bar))
            .child(h_flex().flex_none().w(px(76.)).justify_end().child(size))
            .into_any_element()
    }

    fn adding(&self, palette: &Palette) -> Option<AnyElement> {
        let (path, watch) = self.adding.as_ref()?;
        let clone = watch.clone.lock().unwrap().clone();
        let note = watch.notes.lock().unwrap().last().cloned();
        let (value, progress) = match &clone {
            Some((files, bytes, progress)) => {
                let done = progress.files.load(Ordering::Relaxed);
                let copied = progress.bytes.load(Ordering::Relaxed);
                (
                    Some(done as f32 * 100.0 / (*files).max(1) as f32),
                    format!(
                        "{} / {} 个文件，{} / {}",
                        format::count(done),
                        format::count(*files),
                        format::bytes(copied),
                        format::bytes(*bytes)
                    ),
                )
            }
            None => (None, "正在准备".to_owned()),
        };
        Some(
            v_flex()
                .gap(px(8.))
                .px(px(14.))
                .py(px(12.))
                .rounded(px(10.))
                .bg(palette.hover)
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(Spinner::new().small().color(palette.ink2))
                        .child(
                            text(13., palette.ink)
                                .font_weight(FontWeight::MEDIUM)
                                .child(format!("正在创建 {path}")),
                        )
                        .child(text(12., palette.ink3).child(progress)),
                )
                .child(match value {
                    Some(value) => Progress::new("adding").value(value),
                    None => Progress::new("adding").loading(true),
                })
                .when_some(note, |card, note| {
                    card.child(text(12., palette.ink3).child(note))
                })
                .into_any_element(),
        )
    }

    // ---- inspector -----------------------------------------------------

    fn actions(
        &self,
        worktree: &Worktree,
        details: Option<&Details>,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let path = worktree.path.clone();
        let busy = self.busy.get(&path).copied();
        let waiting = busy.is_some() || self.global_busy.is_some();
        let dirty = details.is_some_and(Details::dirty);
        let ahead = details.and_then(|details| details.ahead).unwrap_or(0);
        let blocked: Option<String> = if let Some(editor) = details.and_then(|d| d.editor.clone()) {
            Some(format!("{editor} 开着，先关掉它"))
        } else if worktree.locked {
            Some("已锁定，先解锁".to_owned())
        } else {
            None
        };

        let mut row = h_flex().gap(px(8.)).flex_wrap();
        let linked = !worktree.is_main && worktree.exists;
        if linked && self.cleanable(worktree) {
            let target = worktree.clone();
            row = row.child(
                Button::new("clean")
                    .small()
                    .primary()
                    .icon(IconName::BrushCleaning)
                    .label("清理")
                    .tooltip("提交都已经合并：删除这个工作区和它的分支")
                    .loading(busy == Some(Busy::Clean))
                    .disabled(waiting)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_clean(target.clone(), window, cx)
                    })),
            );
        } else if linked && worktree.branch.is_some() && ahead > 0 {
            let target = path.clone();
            let hint = if dirty {
                "先提交里面的改动".to_owned()
            } else {
                blocked
                    .clone()
                    .unwrap_or_else(|| "合并进主分支，再删除这个工作区和分支".to_owned())
            };
            row = row.child(
                Button::new("finish")
                    .small()
                    .primary()
                    .icon(IconName::GitMerge)
                    .label("合并并清理")
                    .tooltip(hint)
                    .loading(busy == Some(Busy::Finish))
                    .disabled(waiting || dirty || blocked.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_finish(target.clone(), window, cx)
                    })),
            );
        }
        if worktree.exists {
            let open = path.clone();
            let terminal = path.clone();
            row = row
                .child(
                    Button::new("open")
                        .small()
                        .outline()
                        .icon(IconName::FolderOpen)
                        .accessibility_label("在文件管理器里打开")
                        .tooltip("在文件管理器里打开")
                        .on_click(move |_, _, cx| cx.open_with_system(&open)),
                )
                .child(
                    Button::new("terminal")
                        .small()
                        .outline()
                        .icon(IconName::SquareTerminal)
                        .accessibility_label("在终端里打开")
                        .tooltip("在终端里打开")
                        .on_click(move |_, _, _| open_terminal(&terminal)),
                );
        }
        if linked {
            let lock = path.clone();
            let remove = path.clone();
            let quiet_danger = ButtonCustomVariant::new(cx)
                .foreground(palette.danger)
                .hover(palette.danger.opacity(0.1))
                .active(palette.danger.opacity(0.18));
            row = row
                .child(
                    Button::new("lock")
                        .small()
                        .outline()
                        .accessibility_label(if worktree.locked { "解锁" } else { "锁定" })
                        .icon(if worktree.locked {
                            IconName::LockOpen
                        } else {
                            IconName::Lock
                        })
                        .tooltip(if worktree.locked {
                            "解锁"
                        } else {
                            "锁定，防止被删除"
                        })
                        .loading(busy == Some(Busy::Lock))
                        .disabled(waiting)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_lock(lock.clone(), cx)),
                        ),
                )
                .child(div().flex_1())
                .child(
                    Button::new("remove")
                        .small()
                        .custom(quiet_danger)
                        .icon(IconName::Trash)
                        .label("删除")
                        .tooltip(
                            blocked
                                .clone()
                                .unwrap_or_else(|| "删除这个工作区".to_owned()),
                        )
                        .loading(busy == Some(Busy::Remove))
                        .disabled(waiting || blocked.is_some())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_remove(remove.clone(), window, cx)
                        })),
                );
        }
        row
    }

    fn inspector(
        &self,
        snapshot: &Snapshot,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(worktree) = self
            .selected
            .as_ref()
            .and_then(|path| snapshot.worktrees.iter().find(|w| &w.path == path))
        else {
            return div().into_any_element();
        };
        let details = self.details.get(&worktree.path);
        let usage = self.usage.get(&worktree.path);
        let into = snapshot.into.as_deref().unwrap_or("主分支");

        let copy = worktree.path.clone();
        let mut body = v_flex().p(px(24.)).gap(px(26.)).child(
            v_flex()
                .gap(px(8.))
                .child(
                    text(18., palette.ink)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(worktree.name.clone()),
                )
                .child(
                    h_flex()
                        .items_start()
                        .gap(px(6.))
                        .child(
                            text(12., palette.ink3)
                                .flex_1()
                                .min_w_0()
                                .font_family(palette.mono.clone())
                                .child(worktree.path.display().to_string()),
                        )
                        .child(
                            Button::new("copy")
                                .xsmall()
                                .ghost()
                                .icon(IconName::Copy)
                                .accessibility_label("复制路径")
                                .tooltip("复制路径")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.copy_path(&copy, window, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .pt(px(6.))
                        .child(self.actions(worktree, details, palette, cx)),
                ),
        );

        // What it takes.
        if let Some(usage) = usage {
            let split = usage.split;
            let total = split.total();
            let kinds = parts(&split, palette);
            let mut section = v_flex()
                .gap(px(10.))
                .child(caption("占用", palette))
                .child(
                    h_flex()
                        .items_end()
                        .justify_between()
                        .child(figure(total, 26., 14., palette))
                        .child(
                            run(12., 26., palette.ink3, palette)
                                .child(format!("{} 个文件", format::count(usage.files))),
                        ),
                )
                .child(stack("kinds", &kinds, 8., PANEL_SPAN, palette))
                .child(
                    v_flex().pt(px(2.)).gap(px(7.)).children(
                        kinds
                            .iter()
                            .filter(|part| {
                                part.bytes > 0
                                    && (part.label != "Git" || part.bytes >= GIT_WORTH_SHOWING)
                            })
                            .map(|part| {
                                h_flex()
                                    .gap(px(8.))
                                    .child(swatch(part, palette))
                                    .child(text(13., palette.ink2).flex_1().child(part.long))
                                    .child(
                                        text(13., palette.ink)
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(format::bytes(part.bytes)),
                                    )
                                    .child(
                                        text(12., palette.ink3)
                                            .w(px(36.))
                                            .text_right()
                                            .child(format::percent(part.bytes, total)),
                                    )
                            }),
                    ),
                );
            if !worktree.is_main {
                section = section.child(text(12., palette.ink3).child(format!(
                    "删除它能腾出 {}",
                    format::bytes(total - split.linked)
                )));
            }
            if details.is_some_and(|details| details.reflink) {
                section = section.child(
                    text(12., palette.ink3)
                        .child("这个卷上没改过的文件和源工作区共用数据，实际占用比这里小。"),
                );
            }
            body = body.child(section);

            // The same three columns as the list of worktrees: name, a bar on
            // the scale of the largest entry, size.
            let largest = usage
                .entries
                .first()
                .map_or(1, |entry| entry.split.total())
                .max(1);
            body =
                body.child(
                    v_flex()
                        .gap(px(11.))
                        .child(caption("最大的目录", palette))
                        .children(usage.entries.iter().take(6).enumerate().map(
                            |(index, entry)| {
                                let bytes = entry.split.total();
                                let share = bytes as f32 / largest as f32;
                                h_flex()
                                    .gap(px(8.))
                                    .child(
                                        glyph(
                                            if entry.dir {
                                                IconName::Folder
                                            } else {
                                                IconName::File
                                            },
                                            13.,
                                            palette.ink3,
                                        )
                                        .flex_none(),
                                    )
                                    .child(
                                        text(13., palette.ink)
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(entry.name.clone()),
                                    )
                                    .child(div().flex_none().w(px(96.)).when(
                                        share >= 0.02,
                                        |column| {
                                            column.child(div().w(relative(share.min(1.))).child(
                                                stack(
                                                    ("entry", index),
                                                    &parts(&entry.split, palette),
                                                    6.,
                                                    ENTRY_SPAN * share.min(1.),
                                                    palette,
                                                ),
                                            ))
                                        },
                                    ))
                                    .child(
                                        text(13., palette.ink2)
                                            .flex_none()
                                            .w(px(64.))
                                            .text_right()
                                            .child(format::bytes(bytes)),
                                    )
                            },
                        )),
                );
        } else if let Some(progress) = self.scanning.get(&worktree.path) {
            body = body.child(
                v_flex().gap(px(10.)).child(caption("占用", palette)).child(
                    h_flex()
                        .gap(px(8.))
                        .child(Spinner::new().small().color(palette.ink3))
                        .child(text(13., palette.ink2).child(format!(
                            "正在测量，已读 {} 个文件，{}",
                            format::count(progress.files.load(Ordering::Relaxed)),
                            format::bytes(progress.bytes.load(Ordering::Relaxed))
                        ))),
                ),
            );
        }

        // What git says.
        let fact = |icon: IconName, color: Hsla, title: String, sub: Option<String>| {
            h_flex()
                .items_start()
                .gap(px(10.))
                .child(div().pt(px(2.)).child(glyph(icon, 14., color)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(text(13., palette.ink).child(title))
                        .when_some(sub, |block, sub| {
                            block.child(text(12., palette.ink3).child(sub))
                        }),
                )
        };
        let mut git = v_flex().gap(px(12.)).child(caption("Git", palette));
        git = git.child(fact(
            IconName::GitBranch,
            palette.ink3,
            worktree.label(),
            worktree
                .is_main
                .then(|| "主工作区，其他分支合并到这里".to_owned()),
        ));
        if let Some(details) = details {
            if let Some(commit) = &details.last_commit {
                git = git.child(fact(
                    IconName::GitCommitHorizontal,
                    palette.ink3,
                    commit.subject.clone(),
                    Some(format!(
                        "{} · {}",
                        worktree.head.as_deref().map_or("", model::short_hash),
                        format::ago(commit.time)
                    )),
                ));
            }
            if let (Some(ahead), Some(behind)) = (details.ahead, details.behind) {
                let title = match (ahead, behind) {
                    (0, 0) => format!("和 {into} 一样"),
                    (ahead, 0) => format!("比 {into} 多 {ahead} 个提交"),
                    (0, behind) => format!("比 {into} 少 {behind} 个提交"),
                    (ahead, behind) => format!("比 {into} 多 {ahead} 个提交，少 {behind} 个"),
                };
                git = git.child(fact(
                    if ahead > 0 {
                        IconName::ArrowUp
                    } else if behind > 0 {
                        IconName::ArrowDown
                    } else {
                        IconName::Check
                    },
                    palette.ink3,
                    title,
                    None,
                ));
            }
            git = git.child(if details.dirty() {
                fact(
                    IconName::Pencil,
                    palette.warning,
                    format!("{} 处未提交的改动", details.changed + details.untracked),
                    Some(format!(
                        "{} 个文件有改动，{} 个未跟踪",
                        details.changed, details.untracked
                    )),
                )
            } else {
                fact(
                    IconName::Check,
                    palette.ink3,
                    "没有未提交的改动".to_owned(),
                    None,
                )
            });
            if details.merge_in_progress {
                git = git.child(fact(
                    IconName::GitMerge,
                    palette.danger,
                    "有一次合并没做完".to_owned(),
                    Some("解决冲突后提交，或者 git merge --abort".to_owned()),
                ));
            }
            if let Some(editor) = &details.editor {
                git = git.child(fact(
                    IconName::AppWindow,
                    palette.cache,
                    format!("{editor} 开着"),
                    None,
                ));
            }
            let mut about = details.engines.clone();
            if let Some(filesystem) = &details.filesystem {
                about.push(if details.reflink {
                    format!("{filesystem}（reflink）")
                } else {
                    filesystem.clone()
                });
            }
            if !about.is_empty() {
                git = git.child(fact(IconName::Box, palette.ink3, about.join(" · "), None));
            }
            for problem in &details.problems {
                git = git.child(fact(
                    IconName::TriangleAlert,
                    palette.danger,
                    problem.clone(),
                    None,
                ));
            }
        }
        body = body.child(git);

        div()
            .id("inspector")
            .size_full()
            .overflow_y_scrollbar()
            .child(body)
            .into_any_element()
    }

    fn empty(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (title, detail) = match &self.error {
            Some(error) => ("打不开这个仓库".to_owned(), error.clone()),
            None if self.repo.is_some() => ("正在读取".to_owned(), String::new()),
            None => (
                "打开一个仓库".to_owned(),
                "选仓库里的任意目录，看每个工作区占了多少空间。".to_owned(),
            ),
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .child(glyph(IconName::FolderGit2, 30., palette.ink3))
            .child(
                text(16., palette.ink)
                    .pt(px(6.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                text(13., palette.ink2)
                    .max_w(px(420.))
                    .text_center()
                    .child(detail),
            )
            .when(self.repo.is_none() || self.error.is_some(), |block| {
                block.child(
                    div().pt(px(10.)).child(
                        Button::new("empty-open")
                            .primary()
                            .icon(IconName::FolderOpen)
                            .label("选择仓库")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_repo(cx))),
                    ),
                )
            })
            .into_any_element()
    }
}

impl Render for MultiboxApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::current(cx);
        let header = self.header(&palette, cx);

        let content: AnyElement = match self.snapshot.clone() {
            None => self.empty(&palette, cx),
            Some(snapshot) => {
                let largest = snapshot
                    .worktrees
                    .iter()
                    .filter_map(|worktree| self.usage.get(&worktree.path))
                    .map(|usage| usage.split.total())
                    .max()
                    .unwrap_or(0);
                let rows: Vec<AnyElement> = snapshot
                    .worktrees
                    .iter()
                    .enumerate()
                    .map(|(index, worktree)| {
                        self.row(index, worktree, largest, &snapshot, &palette, cx)
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
                                    .p(px(24.))
                                    .gap(px(16.))
                                    .child(self.overview(&snapshot, &palette, cx))
                                    .child(
                                        card(&palette)
                                            .p(px(6.))
                                            .gap(px(2.))
                                            .child(self.list_head(&snapshot, &palette))
                                            .children(self.adding(&palette))
                                            .children(rows),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w(px(372.))
                            .flex_none()
                            .h_full()
                            .bg(palette.panel)
                            .border_l_1()
                            .border_color(palette.hairline)
                            .child(self.inspector(&snapshot, &palette, cx)),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(true, cx)))
            .bg(palette.canvas)
            .text_color(palette.ink)
            .text_size(px(13.))
            .child(header)
            .child(content)
    }
}
