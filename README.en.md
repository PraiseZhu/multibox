# Multibox

[简体中文](README.md) | English

**`git worktree` for game development: a new worktree is ready to work in, with no reimport and no full rebuild.**

A native `git worktree` is a clean checkout, so Unity reimports every asset and
Rust recompiles every dependency before you can start. `git multibox add` takes
the same arguments as `git worktree add` and makes a normal git worktree, but
brings along the caches that are safe to reuse from the worktree you are in,
such as Unity's `Library/` and Rust's compiled dependencies.

It is for several AI agents or people working on one machine, each in a
worktree of its own, and for trying a branch next to your work while your main
checkout and its editor stay untouched. It knows Unity, Rust (Cargo), pnpm and
TypeScript projects. Any other repository works too, and gets exactly what
`git worktree add` would give it.

## Measured

One Windows workstation, three real projects, against native `git worktree`,
with nothing configured. Each figure covers the steps named, from creating the
worktree.

| Project, steps | NTFS native | NTFS Multibox | Dev Drive native | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Unity 6 game, 104,330 files: create and first launch | 1,165 s | **91 s** | 873 s | **80 s** |
| Disk afterwards | 49.5 GiB | **15.3 GiB** | 47.8 GiB | **2.9 GiB** |
| Rust workspace, 970 packages: create and build the tests | 238 s | **156 s** | 233 s | **148 s** |
| Disk afterwards | 30.2 GiB | **27.5 GiB** | 30.3 GiB | **24.5 GiB** |
| pnpm monorepo with Rust helpers: create, install, type check, dev build, installer | 497 s | **361 s** | 498 s | **326 s** |
| Disk afterwards | 5.2 GiB | **3.0 GiB** | 4.7 GiB | **1.9 GiB** |

- The Unity worktree opened after 1 import instead of 56,961. The Rust
  worktree compiled the workspace's own 22 packages instead of 581.
- On macOS, a 47 GiB Unity project: a worktree in 15 s (49 s natively),
  taking 110 MiB with its `Library/` (8.4 GiB natively, without one).
- Merging gives the same commits as with native worktrees, to the commit id.
- Not faster: on NTFS, pnpm's install takes as long as natively.
  `node_modules/` stays behind, since pnpm installs from a store shared between
  projects and there is little to save.

Every figure and how it was measured: [docs/benchmarks.md](docs/benchmarks.md).

## The window

`git multibox ui` opens a window showing what each worktree really takes on
disk, the space and the builds and imports Multibox saved, and whether a
worktree has uncommitted work or is already merged. New, finish, remove and
lock are buttons, and one cleanup removes merged worktrees and what
interrupted removals left behind, listing everything before it deletes. The
window is in Chinese for now.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/ui-dark.png">
  <img alt="The Multibox window: total disk use, free space and what can be cleaned up at the top, the worktrees and their state in a list, and the selected worktree's breakdown and git details on the right" src="docs/images/ui-light.png">
</picture>

What each part shows: [crates/multibox-ui](crates/multibox-ui/README.md) (in Chinese).

## Install

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/fpgames/multibox/main/install.ps1 | iex
```

macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/fpgames/multibox/main/install.sh | sh
```

Both download the latest [release](https://github.com/fpgames/multibox/releases),
check it against its `SHA256SUMS`, and install the command-line tool and the
window. On macOS they go to `~/.local/bin`, with a hint if that is not on your
PATH. Or build it with Rust:

```bash
cargo install --locked --git https://github.com/fpgames/multibox git-multibox multibox-ui
```

## Use it

Nothing to configure. In any repository:

```bash
git multibox add ../feature -b feature   # instead of git worktree add
git multibox finish ../feature           # merge it back here, then remove the worktree and branch
git multibox remove ../feature           # or throw it away
git multibox ui                          # open the window
git multibox doctor                      # what this machine and repository allow
```

**On Windows, use a Dev Drive.** On NTFS a worktree is mostly a copy; only
caches that are written once are shared, through hard links. On a Dev Drive,
as on macOS, every file is shared until it changes, so a worktree takes
seconds and almost no space. `git multibox init` creates one and moves the
project onto it, once, while you keep working.

To carry more or less, add a `.multibox.toml` to the repository root. The
[guide](docs/guide.md) describes it, how Multibox works and every command.

## Status

Early. Used on Windows and macOS with real Unity 6, Rust and pnpm projects,
and tested on every change on Windows, on NTFS and on a Dev Drive, and on an
Apple Silicon Mac. Linux builds but is untested. Unreal Engine is planned.

## Acknowledgements

The window is built with [GPUI](https://www.gpui.rs), the framework behind the
Zed editor, and [GPUI Kit](https://github.com/longbridge/gpui-kit), with icons
from [Lucide](https://lucide.dev). Every release archive carries the licenses
of all third-party code in `THIRD-PARTY-LICENSES.md`.

## License

MIT or Apache-2.0, at your option.
