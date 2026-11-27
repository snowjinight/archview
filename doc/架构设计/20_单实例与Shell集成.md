# 20 单实例与 Shell 集成

> 半单实例转发、任务栏驻留图标拖入兜底、jump list 最近项登记。
> 三个机制都围绕同一件事：**Windows Shell 对"打开目标"的启动路径不止一条，
> 且目录在部分通道被官方排除**。实现落点：`src/single_instance.rs`、
> `src/ui/taskbar_drop.rs`、`src/recent_docs.rs`。

## 20.1 动机与语义矩阵

拖文件到任务栏图标、SendTo 快捷方式、双击 exe、文件关联双击——对进程来说
**全是"Shell 带路径启动 exe"，无法区分来源**。要实现"拖入直接替换当前打开
的实例"（用户预期），只能做全局的单实例语义；但裸启动（双击 exe）又希望
保留开多窗口的能力。最终定版为**半单实例**：

| 启动方式 | 无实例运行 | 已有实例运行 |
|---|---|---|
| 双击 exe / 任务栏右键图标（裸启动，无目标） | 正常启动 | **照常开新窗口**（不参与转发） |
| 任务栏图标拖入 / 拖到 exe / SendTo / 文件关联 / jump list 最近项（带可打开目标） | 正常启动并打开 | **转发给旧实例替换当前内容**，新进程秒退 |
| 带的参数不是可打开目标（如 .txt） | 开空窗口 | 开新实例（同裸启动） |

"可打开目标"判定复用 `main.rs::initial_archive`（压缩包 / 目录 / 媒体文件，
按 config 扩展名），不另造一套规则。

## 20.2 单实例转发（single_instance.rs）

- **互斥体**：`CreateMutexW("archview_instance")`（无前缀 = 隐式
  `Local\` 会话命名空间，够用且不跨会话泄漏）。**所有 GUI 实例一律认领**
  ——历史 bug：曾把认领放在"带目标启动"分支里，裸启动的实例不占名，
  后续带路径启动检测不到它，转发静默失效。句柄随进程存续不显式关闭。
- **找旧实例窗口**：`EnumWindows` + 按 **exe 文件名**匹配（
  `QueryFullProcessImageNameW` 取完整路径比对基名，ASCII 大小写不敏感）。
  不依赖窗口标题或窗口类名——eframe/winit 的窗口两者都可能变。过滤条件：
  可见 + 有标题（跳过无窗口消息辅助窗）。
- **转发**：命中窗口 → `SendMessageW(WM_COPYDATA)`（dwData=0，cbData=
  UTF-16 路径含 NUL）→ `ShowWindow(SW_RESTORE)` + `SetForegroundWindow`
  + `BringWindowToTop` 尽力前台化 → `exit(0)`。
- **接收端**：taskbar_drop.rs 子类化的 wnd_proc 收 `WM_COPYDATA` → 路径进
  `TASKBAR_DROPS` 队列（与 WM_DROPFILES 同一条）→ UI 每帧 `drain()` 取首条
  → `open_path`，与窗口内拖入完全同链路。
- **接入点**：main.rs 里在 config 加载之后、mpv/日志初始化**之前**调用
  ——转发进程秒退，无窗口闪烁、无日志噪音。config 之后的唯一原因：
  `initial_archive` 的媒体文件判定需要 config 扩展名表。
- 失败语义（都是退回正常启动，不丢本次打开目标）：互斥体创建失败 /
  找不到旧窗口（如旧实例恰在退出）。

## 20.3 任务栏拖入：真实机制与平台实测

**初版设计的假设已被对照实验推翻**。原假设"任务栏把 drop 转发给运行中
窗口（WM_DROPFILES）"在现代系统上不成立。实测矩阵（v1.3.3）：

| 拖入目标 | Win10 22H2 | Win11 |
|---|---|---|
| 未固定的**运行中**按钮 | 红色禁止圆环，松手无反应 | 同左（直接拒绝） |
| 固定图标，应用**未运行** | "固定到 X"提示，松手 → **Shell 带参启动 exe**（无关联弹"打开方式"） | 同左 |
| 固定图标，应用**运行中** | 松手 → **把文件固定进 jump list 固定项**，不启动任何进程 | 同左 |

**运行中分支的固定动作全程在 Shell 内部**（写 CustomDestinations），不给
应用进程发任何消息——无拦截点，"拖上去直接替换打开"在运行中状态下无法
实现（OS 既定语义，微软文档写明的 pin 功能）。二步流程：拖上去固定 →
点固定项 → 带参启动 → 20.2 转发 → 已有实例替换打开。未固定运行中按钮
被拒绝的根因与修复见下。

**Win10 红色拒绝的根因（后记）**：用户回溯发现 v1（尚未引入显式 AUMID）
在 Win10 上拖入显示"固定到"提示（与记事本一致），引入 AUMID 后变红色
拒绝——**显式 AppUserModelID 必须有对应的已安装快捷方式**（快捷方式带
`System.AppUserModel.ID` 属性），Shell 才能把任务栏按钮解析为可固定应用；
无快捷方式的 AUMID 是"孤儿标识"，按钮的拖入/固定归迹失败。修复：
`recent_docs::ensure_start_menu_shortcut` 启动时确保用户级 Start Menu
存在 `archview.lnk`（目标 = 本 exe + PKEY_AppUserModel_ID = AUMID，
已存在不重写）。对照实验矩阵中的记事本行为差异也由此解释：记事本的
包快捷方式自带 AUMID。

taskbar_drop.rs 现状：`WM_DROPFILES` 通道按原假设编写，实测收不到任务栏
投递，保留为防御性兜底（万一第三方工具/未来系统投递仍能接住，成本仅是
子类化 + DragAcceptFiles）；`WM_COPYDATA` 接收端（20.2 的转发落点）是本
模块存在的主要理由。应用侧 `WS_EX_ACCEPTFILES` 经 `GetWindowLong
(GWL_EXSTYLE)` 确认正确设置（0x40110 含 0x10）。

## 20.4 jump list 最近项登记（recent_docs.rs）

Shell 只对**文件关联启动**（双击已注册类型的文件）自动登记最近项；拖入 /
SendTo / 对话框打开一律漏掉。应用在打开成功时主动登记（
`ui/open.rs::apply_open_done` 成功终态，失败不登记）：

- **AUMID 认领**：winit 不设置 AppUserModelID（任务栏按钮按 exe 路径启发
  式分组），带显式 AUMID 的登记无法归入本应用按钮 → 建窗前
  `SetCurrentProcessExplicitAppUserModelID("snowjinight.archview")`。
- **文件**（压缩包/媒体）：`SHAddToRecentDocs(SHARD_APPIDINFOIDLIST)`——
  进「最近」区 + 用户 Recent 文件夹（开始菜单可见），点击按文件关联启动。
- **目录**：`SHAddToRecentDocs` 官方文档明言**文件夹只出现在 Explorer 的
  跳转列表，不出现在任何其它应用的跳转列表**——IDLIST 通道对目录无效。
  走文档指定的唯一正道 `SHARDAPPIDINFOLINK`：手工构造 IShellLink（目标=
  本 exe、参数=带引号目录路径——点击后带参启动 exe，由 20.2 转发接住；
  图标=exe 首图标；`System.Title`=目录名，**硬性要求，缺了条目不显示**，
  经链接的 IPropertyStore 写入）。

## 20.5 windows-sys 裸指针 COM 调用纪律

windows-sys 不导出 COM 接口 vtable 与 GUID 常量，recent_docs.rs 手工调用。
两条硬纪律（都实测踩过坑）：

1. **GUID 禁止凭记忆手写**。来源只能是 SDK 头文件（propsys.h 等）或
   windows-rs 生成码（本地 `~/.cargo/registry/src/.../windows-0.62.2`）。
   实测教训：`IID_IPropertyStore` 记忆版后半段错误 → QI 恒
   `E_NOINTERFACE` → `System.Title` 没写入 → 目录条目不显示。
2. **虚表槽步进必须用 `usize`（8 字节）**。`core::ffi::c_void` 是 ZST，
   对 `*const c_void` 做 `add(idx)` 原地踏步，所谓"槽 20"实际取到槽 0，
   按 SetPath 的签名调 QueryInterface → 参数错位 → release 下 access
   violation **闪退且 panic 钩子拦不住**（钩子是 Rust 层，硬 AV 不经它）。
   正确写法见 `recent_docs.rs::vtable_slot`。槽位序号按 shobjidl_core.h
   方法顺序（IShellLinkW：QI=0 / SetArguments=11 / SetIconLocation=17 /
   SetPath=20；IPropertyStore：QI=0 / Release=2 / SetValue=6 / Commit=7）。

回归测试（`cargo test recent_docs`）：

- `recent_docs_smoke_no_crash`：目录（IShellLink 通道）+ 文件（PIDL 通道）
  完整登记不崩溃（测试线程自建 STA COM，winit 不在场）。
- `dir_link_roundtrip_keeps_arguments`：链接经 IPersistFile 存盘重载，
  目标 exe 与带引号参数（含空格）逐字节保留——守住"Shell 激活时参数不丢"
  这一转发有效性的前提。

## 20.6 与其它链路的关系

- **tidy / encode CLI**：main.rs 命令行分发在转发逻辑之前，命中即 exit，
  不受单实例影响。
- **SendTo / 拖到 exe / 文件关联**：均为"带路径启动"，自动获得 20.2 的
  转发语义，无需各自处理。
- **裸启动多窗口**：设计决策（用户要求保留）；窗口间无内容同步，最近打开
  的窗口在任务栏分组中按 AUMID 归并为一组。
