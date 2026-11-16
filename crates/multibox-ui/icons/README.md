# 图标

两个工作区，上面压着一块它们共用的缓存，画成玻璃的，因为它不单独占空间。源文件是这里的三个 SVG：

| 文件 | 用在 |
| --- | --- |
| `multibox.svg` | Windows 40 px 及以上 |
| `multibox-small.svg` | Windows 16–32 px，同一个图案画得更粗 |
| `multibox-macos.svg` | macOS，按 Apple 的 824/1024 网格，带投影 |

改了 SVG 后运行 `cargo run -p multibox-ui --example icons`，重新生成 `multibox.ico`、`multibox-macos.png` 和 `multibox.icns`。Windows 上 `build.rs` 把 `.ico` 编进程序，资源管理器、标题栏和任务栏都用它；macOS 上程序启动时把 PNG 交给程序坞。`.icns` 留给以后打包成 `.app` 用。
