# multibox-ui

`git multibox` 的桌面界面：一眼看清一个仓库开了哪些 worktree、每个占多少磁盘、空间花在哪，并直接在界面里清理。用 [GPUI](https://www.gpui.rs)（Zed 编辑器的 GPU 加速 UI 框架）和 [GPUI Kit](https://github.com/longbridge/gpui-kit) 组件库写成。

## 运行

发布包和安装脚本会把 `multibox-ui` 和 `git-multibox` 装在同一个目录。在仓库里：

```bash
git multibox ui            # 打开当前仓库，终端马上就能接着用
git multibox ui ../other   # 或者指定仓库里的任意目录
```

也可以直接运行 `multibox-ui [<目录>]`；不在仓库里时窗口会让你选一个。从源码运行：

```bash
cargo run -p multibox-ui --release -- <仓库里的任意目录>
```

## 能看到什么

- **总览**：worktree 数量、所有 worktree 的文件大小之和、按硬链接去重后的实际占用、所在磁盘剩余空间和文件系统（reflink / 复制模式）、可回收的空间。
- **空间分布**：按 worktree 和按类型两条分段条。
- **每个 worktree 一张卡片**：分支、是否主工作区、锁定、改动数、未跟踪数、是否已合并、编辑器是否开着；占用拆成四类；比主分支领先/落后多少、最近一次提交、识别到的工程（Unity / Rust / pnpm / TypeScript）。
- **右侧详情**：选中 worktree 最大的目录（带二级子目录）、Git 状态、文件系统和工程。

占用分四类：

| 类型 | 含义 |
| --- | --- |
| 源码 | git 跟踪的文件（读 worktree 的 index 判断） |
| 缓存 / 未跟踪 | 其余文件，主要是 Multibox 带过去的引擎缓存（Library/、target/、node_modules/ …） |
| 硬链接共享 | 链接数大于 1 的文件。NTFS 上 Multibox 用硬链接共享只写一次的缓存；按文件 ID 去重后计入“实际占用” |
| Git 数据 | 主工作区的 `.git`，所有 worktree 共用 |

在 Dev Drive（ReFS）和 APFS 上，没改过的数据通过 reflink 共享数据块，文件系统不提供逐文件的共享量，界面显示的是文件大小，实际占用以磁盘剩余空间为准。

## 能做什么

| 按钮 | 做的事 |
| --- | --- |
| 新建 worktree | `git multibox add <路径> [-b <分支>]`，带进度条 |
| 合并并清理 | `git multibox finish`：合并进主工作区当前分支，再删 worktree 和分支；有冲突时停下，什么都不删 |
| 清理 / 清理已合并 | 提交都已在主分支里、没有改动、没锁、没开编辑器的 worktree：删除它和它的分支（`git branch -d`，没合并的分支删不掉） |
| 删除 | `git multibox remove`；有改动时确认框会说明并改为强制删除 |
| 锁定 / 解锁 | `git worktree lock` / `unlock` |
| 清空残留 | 删掉中断的删除留下的 `.multibox-trash-*` 目录 |
| 清除失效记录 | `git worktree prune` |
| 打开 / 终端 / 复制路径 | 在文件管理器、终端里打开，或复制路径 |

所有删除类操作都先弹确认框。编辑器开着的 worktree 不能删除或合并清理，锁定的要先解锁。

快捷键：`Ctrl+R`（macOS 上 `Cmd+R`）或 `F5` 刷新并重新测量。
