//! `multibox-ui`: a window onto a repository's worktrees.
//!
//! Shows every worktree with what it takes on disk and what that space is
//! made of, and offers Multibox's own operations as buttons: remove, finish
//! (merge back and clean up), lock, prune and emptying leftover trash.
//!
//! Usually started by `git multibox ui`, from the same directory. Run alone:
//! `multibox-ui [<directory in a repository>]`, defaulting to the
//! current directory. Without a repository the window asks for one.

// No console window behind the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod format;
mod model;
mod ops;
mod theme;
mod usage;
mod view;

use gpui_kit::{
    App, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use std::path::PathBuf;

fn repository_argument() -> Option<PathBuf> {
    let candidate = match std::env::args_os().nth(1) {
        Some(arg) => PathBuf::from(arg),
        None => std::env::current_dir().ok()?,
    };
    // Only a directory inside a repository is taken; anything else opens the
    // window empty, asking for one.
    multibox_core::git::Repo::discover(&candidate)
        .ok()
        .map(|repo| repo.worktree)
}

fn main() {
    let repo = repository_argument();
    // Windows refuses to delete or rename a directory that a process works in,
    // and this window removes worktrees. Work from where the program lives.
    if let Some(home) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        let _ = std::env::set_current_dir(home);
    }
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            let refresh = if cfg!(target_os = "macos") {
                "cmd-r"
            } else {
                "ctrl-r"
            };
            cx.bind_keys([
                KeyBinding::new(refresh, view::Refresh, None),
                KeyBinding::new("f5", view::Refresh, None),
            ]);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1360.), px(880.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Multibox".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(980.), px(600.))),
                ..Default::default()
            };
            let opened = gpui_kit::open_window(options, cx, move |window, cx| {
                theme::install(window, cx);
                cx.new(|cx| view::MultiboxApp::new(repo, window, cx))
            });
            if let Err(err) = opened {
                eprintln!("multibox-ui: cannot open a window: {err}");
                cx.quit();
            }
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
