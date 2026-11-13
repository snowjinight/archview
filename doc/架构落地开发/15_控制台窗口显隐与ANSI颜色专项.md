# §15 控制台窗口显隐与 ANSI 颜色专项

> 专题：exe 是 console 子系统，Windows 启动必分配黑框控制台——GUI 用户嫌烦、
> CLI/排障场景又要看日志；同时 tracing 的 ANSI 颜色在 legacy conhost 显示为乱码。
> 实现：`src/console.rs` + `src/main.rs` + `src/ui/nav.rs`（F12）。

## 1. 现象与根因

| 现象 | 根因 |
|---|---|
| 双击 exe 启动后一直挂着黑框 cmd 窗口，而 Chrome 等软件没有 | PE 头 Subsystem 字段：`console` 子系统（Rust 默认）Windows 必分配控制台；`windows` 子系统不分配。代码无法阻止创建，只能事后隐藏 |
| 日志颜色在 VSCode/WT 终端正常，双击启动的黑框里是 `←[32m…` 乱码 | `tracing_subscriber::fmt()`（`ansi` feature）**无条件**输出 ANSI 转义序列，不做能力探测；legacy conhost 默认未开启 Virtual Terminal Processing（VT），把 `\x1b[32m` 打成字面量 |

## 2. 方案取舍

- **不改子系统**：保持 console 子系统。改 `windows_subsystem = "windows"` 会让
  CLI 子命令（tidy/encode）失去输出，且本方案运行时隐藏已解决黑框问题
  （encode/vartidy CLI 里的 AttachConsole 逻辑保留作未来切换的预留）；
- **不用 release/debug 区分**（用户要求）：按**启动来源**判定——
  从终端启动（cmd/powershell/wt/…）说明用户想看日志 → 保留；
  双击/资源管理器/sendto/拖拽 → 隐藏。判定失败保守保留（宁多显示不错隐藏）；
- **F12 兜底**：控制台被隐藏后日志仍在写缓冲区，GUI 内按 F12 随时调出/收回，
  任何模态（config 编辑器/对比/密码框）下都可用。

## 3. 实现要点（src/console.rs）

- **VT 开启** `enable_ansi()`：对 stdout/stderr 各做
  `GetConsoleMode` → `SetConsoleMode(mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING)`。
  在 `main()` 最先调用（CLI 分发前），tidy/encode 与 GUI 都受益；失败静默；
- **来源判定** `launched_from_terminal()`：
  1. `CARGO` 环境变量存在 → cargo run 开发场景，视为终端启动；
  2. ToolHelp 快照（`TH32CS_SNAPPROCESS`）查自身 PID → 父 PID，
     `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` +
     `QueryFullProcessImageNameW` 取父镜像名，basename 小写后匹配
     shell/终端名单（cmd/powershell/pwsh/wt/bash/wsl/…）。
     终端模拟器（wt/conhost）之下必然是 shell 进程，匹配 shell 即可覆盖全场景；
- **显隐** `set_visible/toggle/is_visible`：`GetConsoleWindow` +
  `ShowWindow(SW_HIDE/SW_SHOW)`，最小化时 `SW_RESTORE`；
- **F12 接线**：`ui/nav.rs handle_shortcuts()` **最顶部**（先于一切早退分支，
  覆盖所有模态），`ctx.input(key_pressed(F12))` → `console::toggle()`；
- **隐藏时机**：`main()` 中 CLI 分发之后、config 加载之前——黑框在 GUI
  窗口创建前消失，不闪屏。

坑：`GetCurrentProcessId` 在 `Win32::System::Threading` 模块（不是
SystemInformation）；windows-sys 新特性依赖：
`Win32_System_Diagnostics_ToolHelp`、`Win32_System_Threading`。

## 4. 测试方法

- `cargo check` 编译通过；
- 终端启动（cmd / powershell / cargo run）：控制台保留，颜色正常渲染；
- 双击 target\debug\archview.exe：黑框消失（或一闪即隐），GUI 正常；
- GUI 内按 F12：黑框调出且继续滚动日志，再按 F12 收回；
- config 编辑器/对比模式/搜索框聚焦状态下按 F12 仍生效；
- `archview tidy --help`、`archview encode` CLI 输出不受影响。
