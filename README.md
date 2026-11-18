# Multibox

简体中文 | [English](README.en.md)

**给游戏开发用的 `git worktree`：新工作区一建好就能干活，不用等重新导入、重新编译。**

原生 `git worktree` 建出来的是一份干净的检出，Unity 要把所有资源重新导入一遍，Rust 要把所有依赖重新编译一遍。`git multibox add` 的参数和 `git worktree add` 一样，建出来的也是普通的 git 工作区，但会把当前工作区里能安全复用的缓存一起带过去，比如 Unity 的 `Library/` 和 Rust 依赖的编译产物。

适合一台机器上多个 AI 代理或多个人同时干活，每个一个工作区；也适合在旁边试一个分支，主工作区和开着的编辑器都不受影响。它认识 Unity、Rust（Cargo）、pnpm 和 TypeScript 工程，其他仓库也能用，效果和 `git worktree add` 一样。

## 实测

Windows 和 macOS 上各测了 Unity、Rust、pnpm 三类真实工程，和原生 `git worktree` 对比，没做任何配置。时间从创建工作区算起，到跑完所列步骤为止。

### Windows

一台工作站（32 线程、61 GiB 内存、Gen4 NVMe），普通 NTFS 盘和 Dev Drive 各测一遍。

| 工程与步骤 | NTFS 原生 | NTFS Multibox | Dev Drive 原生 | Dev Drive Multibox |
| --- | --- | --- | --- | --- |
| Unity 6 游戏，104,330 个文件：创建并首次打开 | 1,165 秒 | **91 秒** | 873 秒 | **80 秒** |
| 之后占用的磁盘 | 49.5 GiB | **15.3 GiB** | 47.8 GiB | **2.9 GiB** |
| Rust 工作区，970 个包：创建并编译测试 | 238 秒 | **156 秒** | 233 秒 | **148 秒** |
| 之后占用的磁盘 | 30.2 GiB | **27.5 GiB** | 30.3 GiB | **24.5 GiB** |
| 带 Rust 组件的 pnpm monorepo：创建、装依赖、类型检查、开发构建、打安装包 | 497 秒 | **361 秒** | 498 秒 | **326 秒** |
| 之后占用的磁盘 | 5.2 GiB | **3.0 GiB** | 4.7 GiB | **1.9 GiB** |

### macOS

APFS 上文件在被修改之前都是共享的，效果和 Windows 的 Dev Drive 一样，不用另外设置。

| 工程与步骤 | 原生 | Multibox |
| --- | --- | --- |
| Unity 6 工程，202,929 个文件、47 GiB（M4 Mac mini）：创建 | 49 秒 | **15 秒** |
| 之后占用的磁盘 | 8.4 GiB，还不含 `Library/` | **110 MiB，含 `Library/`** |
| 首次打开 | 未测 | 82 秒，只导入 1 个资源 |
| Rust 工作区，Multibox 自己的 38 个包（Mac Studio）：创建并编译测试 | 2.2 秒，编译 38 个包 | **1.4～1.7 秒，编译 3 个包** |
| 同一个 pnpm monorepo，2,328 个包（Mac Studio）：创建并装依赖 | 12 秒 | 12 秒 |
| 装完依赖后占用的磁盘 | 1.5 GiB | **1.1 GiB** |

macOS 上的工程和步骤与 Windows 不完全相同：Rust 用的是 Multibox 自己的小工作区，pnpm 只测到装完依赖，Unity 原生的首次打开还没测。

- Unity 工作区首次打开只导入了 1 个资源，原生是 56,961 个；Rust 工作区只编译项目自己的 22 个包，原生是 581 个。
- 合并的结果和原生工作区完全一样，连提交 id 都相同。
- 没变快的：pnpm 装依赖的耗时在两个系统上都和原生一样。`node_modules/` 不带过去：pnpm 本来就从多个工程共用的 store 安装，带了也省不了多少。

完整数据和测法见 [docs/benchmarks.md](docs/benchmarks.md)（英文）。

## 工作区管理器

`git multibox ui` 打开一个窗口：每个工作区实际占多少磁盘，Multibox 省下了多少空间、多少次编译和导入，有没有没提交的改动，是不是已经合并。新建、合并、删除、锁定都能直接点；已合并的工作区和中断留下的残留目录可以一键清理，删之前都会先列出要删什么。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/ui-dark.png">
  <img alt="Multibox 工作区管理器：顶部是总占用、磁盘剩余和可清理的空间，中间是工作区列表和各自的状态，右侧是选中工作区的占用明细和 Git 信息" src="docs/images/ui-light.png">
</picture>

每个部分的含义见[管理器说明](crates/multibox-ui/README.md)。

## 安装

Windows，在 PowerShell 里运行：

```powershell
irm https://raw.githubusercontent.com/fpgames/multibox/main/install.ps1 | iex
```

macOS：

```bash
curl -fsSL https://raw.githubusercontent.com/fpgames/multibox/main/install.sh | sh
```

安装脚本会下载最新的[发布版本](https://github.com/fpgames/multibox/releases)，核对 `SHA256SUMS` 后装好命令行工具和管理器。macOS 装在 `~/.local/bin`，不在 PATH 里时会提示怎么加。也可以用 Rust 自己编译：

```bash
cargo install --locked --git https://github.com/fpgames/multibox git-multibox multibox-ui
```

之后可在任意目录运行 `git multibox update`，把命令行和管理器一起更新到最新正式版；`git multibox update --check` 只检查版本。更新会核对 `SHA256SUMS`，安装到当前命令所在的目录；Windows 上先关闭管理器。下载使用系统自带的 `curl` 和 `tar`，不需要安装 Rust。

## 用法

不需要配置。在任意仓库里：

```bash
git multibox add ../feature -b feature   # 代替 git worktree add
git multibox finish ../feature           # 合并回当前分支，再删掉工作区和分支
git multibox remove ../feature           # 或者直接丢弃
git multibox ui                          # 打开工作区管理器
git multibox doctor                      # 这台机器和这个仓库能用上什么
git multibox disk                        # 工程所在的卷、Dev Drive 的虚拟盘文件和迁移留下的原工程
```

**Windows 上建议用 Dev Drive。** NTFS 上工作区里的文件大多要复制，只有只写一次的缓存用硬链接共享；Dev Drive 和 macOS 上，文件在被修改之前都是共享的，建一个工作区只要几秒，几乎不占空间。`git multibox init` 会创建 Dev Drive 并把工程搬过去，只需做一次，搬的时候不耽误干活。

想多带或少带一些东西，在仓库根目录放一个 `.multibox.toml`。写法、原理和全部命令见[使用指南](docs/guide.md)（英文）。

## 状态

早期版本。已在 Windows 和 macOS 上用于真实的 Unity 6、Rust 和 pnpm 工程，每次改动都在 Windows（NTFS 和 Dev Drive）和 Apple 芯片的 Mac 上跑测试。Linux 能编译，但没有测试；Unreal Engine 在计划中。

## 致谢

管理器用 [GPUI](https://www.gpui.rs)（Zed 编辑器的界面框架）和 [GPUI Kit](https://github.com/longbridge/gpui-kit) 写成，图标来自 [Lucide](https://lucide.dev)。所有第三方依赖的许可证都附在每个发布包的 `THIRD-PARTY-LICENSES.md` 里。

## 许可证

MIT 或 Apache-2.0，任选其一。
