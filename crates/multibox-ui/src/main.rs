//! `multibox-ui`: a window onto a repository's worktrees.
//!
//! Shows every worktree with what it takes on disk and what that space is
//! made of, and offers Multibox's own operations as buttons: remove, finish
//! (merge back and clean up), lock, prune and emptying leftover trash.
//!
//! Usually started by `git multibox ui`, from the same directory. Run alone:
//! `multibox-ui [<directory in a repository>]`, defaulting to the
//! current directory, then the last successfully opened repository. Without
//! either repository the window asks for one.

// No console window behind the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod format;
mod model;
mod ops;
mod state;
mod theme;
mod usage;
mod view;

use gpui_kit::{
    App, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, point, px,
    size,
};
use std::path::PathBuf;

fn repository_argument() -> Option<PathBuf> {
    let argument = std::env::args_os().nth(1).map(PathBuf::from);
    state::startup_repository(argument.as_deref(), std::env::current_dir().ok().as_deref())
}

/// The program runs as a bare executable, with no app bundle to take an icon
/// from, so the Dock and the app switcher would show a generic one. On
/// Windows the icon is built into the program instead; see `build.rs`.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let png = NSData::with_bytes(include_bytes!("../icons/multibox-macos.png"));
    if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &png) {
        // SAFETY: on the main thread, and the image is not nil.
        unsafe {
            NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&icon));
        }
    }
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
            #[cfg(target_os = "macos")]
            set_dock_icon();
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
                // On Windows and macOS the window's top bar is its title bar,
                // which a system one would only repeat above it; see
                // `view::OWN_TITLE_BAR`. Linux keeps the window manager's.
                titlebar: Some(TitlebarOptions {
                    title: Some("Multibox".into()),
                    appears_transparent: view::OWN_TITLE_BAR,
                    traffic_light_position: cfg!(target_os = "macos")
                        .then(|| point(px(20.), px((view::TOP_BAR - 14.) / 2.))),
                }),
                app_owns_titlebar_drag: view::OWN_TITLE_BAR,
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
