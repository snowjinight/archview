# §15 日志与控制台子系统专项（原：控制台窗口显隐）

> 专题：GUI 侧日志从哪里来、怎么不打扰用户。演进：console 子系统 + 运行时
> 隐藏黑框（旧方案，有父进程判定竞态）→ **windows 子系统 + 文件日志管线**
> （现方案，彻底无黑框）。实现：`src/console.rs` + `src/logsink.rs` +
> `src/main.rs` + `src/ui/nav.rs` / `dialogs.rs`（F12 日志面板）。

## 1. 现象与根因（旧方案遗留记录）

| 现象 | 根因 |
|---|---|
| 双击 exe 启动后一直挂着黑框 cmd 窗口，而 Chrome 等软件没有 | PE 头 Subsystem 字段：`console` 子系统（Rust 默认）Windows 必分配控制台；`windows` 子系统不分配 |
| 日志颜色在 VSCode/WT 终端正常，黑框里是 `←[32m…` 乱码 | tracing_subscriber（ansi feature）无条件输出转义序列；legacy conhost 未开 VT 时打成字面量 |
| 文件关联启动黑框**有时**漏藏，时好时坏 | 旧方案按父镜像名判定启动来源，是竞速：关联启动的父进程（shell 代理）瞬时退出、PID 被复用，复用进程恰为终端镜像名 → 误判保留黑框。**现方案切 windows 子系统后此判定整体删除，问题不复存在** |

## 2. 方案取舍（演进）

旧方案：console 子系统 + 按启动来源运行时隐藏（终端保留/双击隐藏 + F12 调出）。
当时不改子系统的顾虑是"CLI 子命令失去输出"——但 encode/vartidy 的 `Out`
壳（AttachConsole + CONOUT$ 直写 + 报告文件/msgbox 兜底）已完整解决 CLI
输出，顾虑不再成立，于是彻底切换：

- **release = `windows` 子系统**（`#![cfg_attr(not(debug_assertions),
  windows_subsystem = "windows")]`）：不分配控制台，UI 全程零黑框；
- **debug 保持 console 子系统**：cargo run 终端彩色直显日志（开发体验不变）；
- **GUI 日志出口 = logsink**：内存环形缓冲（F12 界面面板即时看）+ 后台线程
  定时落盘 archview.log；
- **CLI 保持控制台 attach**（用户要求）：终端启动 AttachConsole 内联输出；
  附加失败（SendTo/双击，父进程无控制台）AllocConsole 自分配黑窗（恢复
  console 子系统时代的进度体验，进程退出自动关闭）。

## 3. 实现要点

### 3.1 日志管线（src/logsink.rs）

- **环形缓冲**：`VecDeque<String>` 定容 600 行（超长行截 1500 字节），
  fmt 层 `with_ansi(false)` 写入（缓冲与落盘都要纯文本）；跨 write 调用
  的未收尾行用 `cur: Vec<u8>` 累积、`\n` 时 `from_utf8_lossy` 收行；
- **定时落盘**：后台线程 `wait_timeout(5s)` 或增量攒满 64KB 提前唤醒，
  `BufWriter` append 追加 archview.log（exe 同目录）；进程退出最多丢
  最近 5s 未落盘增量（诊断日志可接受）；
- **轮转**：启动时文件 >4MB → 改名 `archview.log.old`（覆盖上一代）；
- **MakeWriter**：孤儿规则不允许为 `Arc<LogSink>` 直接实现外部 trait，
  用本地 newtype `SinkWriter(pub Arc<LogSink>)` 包装；
- **全局访问**：`OnceLock<Arc<LogSink>>`，UI 免穿透取快照
  （`snapshot()/file_path()/clear_view()`）；
- **tracing 接线**（main.rs）：`registry().with(EnvFilter)
  .with(fmt 层 → SinkWriter)` + debug 构建追加 stderr 层（彩色）。
  需要 `use tracing_subscriber::prelude::*`（SubscriberExt/Layer）。

### 3.2 控制台工具（src/console.rs）

只留两个函数，父进程判定/显隐逻辑整体删除：

- `enable_ansi()`：VT 开启（debug 终端与 CLI 自分配黑窗受益），无控制台
  时静默（release GUI 常态）；
- `fatal_msgbox()`：windows 子系统下 eprintln/panic 不可见，config 加载
  失败等启动期致命错误先弹窗再 panic。

### 3.3 CLI 控制台接入（encode/cli.rs、vartidy/mod.rs 的 `Out::attach`）

1. `GetConsoleWindow` 非空（debug）→ println 直走；
2. `AttachConsole(ATTACH_PARENT_PROCESS)`（终端启动）→ `File::create("CONOUT$")`
   直写屏幕（绕过被重定向的 std 句柄，输出一定落在真实终端）；
3. 附加失败（SendTo/双击，父进程无控制台）→ `AllocConsole()` 自分配
   黑窗：encode 进度 `\r` 刷新可见、tidy 报告同步落文件，结束时 msgbox
   汇总，进程退出窗口自动关闭。

坑：管道/重定向捕获不到 CONOUT$ 输出（写的是屏幕缓冲不是 stdout 句柄），
自动化验证要靠真实终端或退出码。

### 3.4 UI 日志面板（ui/nav.rs F12 + dialogs.rs log_window）

- F12 在 `handle_shortcuts()` 最顶部切换 `log_open`（先于一切早退分支，
  覆盖所有模态）；
- 窗口内容：`logsink::snapshot()` 拉环形缓冲，`ScrollArea::show_rows` +
  `stick_to_bottom(follow)`（跟随底部默认开，手动开关语义 + 挤出补偿见
  §5.3）；布局为**全宽贴底停靠**
  （`Window::fixed_rect`：与视口同宽、下缘对齐、高约 38%，不可拖动/缩放，
  视口尺寸取 `ctx.input(|i| i.raw.screen_rect)`）；工具行：跟随开关 /
  复制全部（`ctx.copy_text`）/ 用记事本打开 archview.log / 清屏
  （仅清缓冲视图，落盘不动）/ 收起；
- **收起态**：egui 自带折叠按钮禁用（`.collapsible(false)`——它会把窗口
  缩成左缘标题条挡住文件列表），改用「收起 ▶」按钮 → `log_collapsed=true`，
  只渲染右缘中部小标签（`Area + Align2::RIGHT_CENTER`，点击展开）；
- F12 三态：关闭 → 展开 → 收起态按 F12 展开；展开时按 F12 = 关闭。
  面板开关日志用 `debug!` 级（交互噪声，info 档刷屏）。

## 4. 测试方法（2026-11-22 实测）

- `cargo build --release` + `cargo test`（208 通过）；
- explorer 启动 release exe：枚举进程窗口，`ConsoleWindowClass` 计数 = 0
  （彻底无黑框）；8s 后 archview.log 出现且首行为启动横幅；
- 终端 `archview.exe encode --help`：输出直写终端屏幕（重定向文件为空
  即证明走 CONOUT$）；exit 0；
- 无控制台父进程（WScript.Shell.Run）跑 `encode --help`：AllocConsole
  兜底，exit 0 不崩；
- GUI 内按 F12：日志窗口开合、跟随/复制/清屏可用；
- config.toml 破坏后启动：弹窗报 TOML 错误后 panic（不再静默闪退）。

## 5. 修复记录：跟随底部失效 + 上翻时正文"向下滚 N 行"（2026-11-26）

### 5.1 现象

1. 勾选「跟随底部」后手动上翻日志，新日志到达（如 3 行）时视图不跳到底部，
   而是当前阅读位**向下滚 3 行**（正文整体上顶 3 行），滚动距离恒等于新日志行数；
2. 取消勾选后现象与勾选时**一样**——复选框看起来完全不起作用。

### 5.2 根因（两个症状同根）

- **主因：满环挤出造成假滚动**。环形缓冲满 600 行后，每来 1 行就
  `pop_front` 挤掉顶行（`push_bytes` / `push_panic` 两处），快照整体索引
  前移；而 egui 的滚动 `State.offset` 是"距内容顶部的像素距离"，无人
  补偿 → 同一段正文在视口中的位置整体上移 N 行，视觉上=向下滚 N 行。
  该现象与 `stick_to_bottom` 勾选与否**无关**，故复选框看似无效；
- **次因：egui stick 语义与预期不符**。`stick_to_bottom(true)` 只在
  "贴着底部（`scroll_stuck_to_end=true`）"时才跟随（scroll_area.rs
  `if stick_to_end[d] && state.scroll_stuck_to_end[d]`）——上翻后
  stuck=false，新日志到达不会跳底。即 egui 语义是"贴底才跟随"，而 UI
  语义应为"复选框是手动开关：勾选=始终跟随，不勾选=冻结阅读位"。

### 5.3 修复

- **logsink.rs**：`Inner` 增加 `evicted_total: u64` 累计挤出行数
  （单调递增，`pop_front` 两处 +1），新增 `pub fn evicted_total()` 读数
  （毒化恢复同 push_bytes）；
- **dialogs.rs log_window**（`show_rows` 之后，重写持久化 State）：
  - 勾选跟随：仅未贴底时把 `st.offset.y` 钉到 `available`（=
    content_size − 视口高，恰为 max_offset）并 `State::store`——下一帧
    直接渲染底部且 stuck 重算为 true，egui 原生 stick 接管；上翻后新
    日志到达也回底部。**踩坑（2026-11-26 二次修复）**：首版每帧无条件
    覆写 `offset = content_size.y`（超出 max_offset 一个视口高），而
    egui 的 clamp 在 `end()` 里、发生在渲染**之后** → 下一帧渲染时
    offset 仍超界，内容整体平移出裁剪区（整帧空白），再 clamp 回正常
    又被覆写超界……逐帧交替 = 日志疯狂闪动后消失。教训：**覆写
    ScrollArea offset 只能写 max_offset（content_size − inner_rect 尺寸），
    且仅在未贴底时写**，渲染帧用的就是存进去的值，不能指望 end() 兜底；
  - 不勾选：仅当本帧有挤出（帧差值）且不在底部（`offset < available`
    - 0.5 容差）时补偿 `offset -= 挤出数 × 行距`（行距=行高+
    item_spacing，与 show_rows 内部换算同款），阅读位纹丝不动；贴底时
    无需补偿（视口本就罩住最新行）；
  - 帧差值记录在 `App.log_evicted`（须在取 `log_follow` 可变借用**前**
    计算——同属 self，否则借用冲突）。

### 5.4 教训

- egui `ScrollArea` 的滚动 offset 是"距内容顶的像素距离"，**内容顶部
  变化（挤出/删除/收缩）必须自行补偿 offset**，否则视觉上内容会平移；
- `stick_to_bottom` ≠ "始终跟随"，是"贴底才跟随"；要做手动开关语义
  （勾选=无条件跳底），须每帧显式改写持久化 State（`show_rows` 返回值
  `ScrollAreaOutput.state` + `State::store(ctx, id)` 是官方公开路径，
  私有字段 stuck/offset_target 会随整个 State 原样带回，无信息丢失）；
- 同一文件的并行 Edit 会静默丢失改动（构造器初始化漏一处 → E0063），
  同文件多处修改必须逐条串行。

