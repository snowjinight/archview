# Archview — 压缩包媒体播放器

直接预览播放压缩包内媒体的桌面工具：无需完整解压，打开 ZIP / 7Z / RAR 压缩包即可浏览和播放其中的视频与图片，支持加密压缩包（密码输入 + 会话内记忆）。

## 功能概览

| 能力 | 说明 |
|---|---|
| 压缩格式 | ZIP（含 deflate/store）、7Z、RAR（含分卷 RAR） |
| 流式播放 | ZIP 直接边读边播（zvf:// 自定义流协议），不用先解压 |
| 后台解压 | 7Z / RAR 自动后台解压到缓存目录，完成后无缝播放 |
| 加密支持 | 加密头（7Z/RAR）打开时输密码；内容加密（ZIP AES 等）播放时输密码；多编码候选兼容汉字密码；会话内密码记忆（切换包后切回不用重输） |
| 文件列表 | 视频 / 图片两个标签页，序号 `[当前/总数]` 显示，支持搜索过滤，多级子目录平铺展示 |
| 连续看包 | 翻到本包最后一项再往后翻（PgDn / 滚轮 / ">" 按钮）时，弹出同目录压缩包列表（默认选中下一个，末包循环回第一个；分卷 RAR 只列首卷），点选任意包打开；本会话看过的包灰色显示；`ui.ask_next_archive` 可关 |
| 播放控制 | mpv 风格快捷键、底部全宽进度条拖动 seek、音量调节、静音提示 |
| 图片自动播放 | PPT 式自动翻页（控制栏播放/暂停按钮或空格切换） |
| 全屏 | 双击画面或回车进入，Esc 退出，全屏内快捷键照常可用 |
| 缓存管理 | 手动清理/打开目录按钮 + 可配置的退出/切换清理 + LRU 容量上限；缓存按包路径稳定命名，切回重开均可命中 |
| 配置集中 | 所有可变项集中在 config.json，代码零硬编码路径 |

## 快捷键

| 按键 | 功能 |
|---|---|
| 空格 | 暂停 / 播放（图片播放中 = 自动翻页开关） |
| ← / → | 后退 / 前进 5 秒 |
| Shift + ← / → | 后退 / 前进 1 秒（精确） |
| ↑ / ↓ | 音量 ±5（调整时自动解除静音） |
| M | 静音切换（静音时音量条旁显示红色"静音"标识，点击可解除） |
| PgUp / PgDn 或 鼠标滚轮（视频区） | 上一个 / 下一个媒体（同类型内导航，滚轮有节流间隔） |
| 回车 或 双击画面 | 全屏切换 |
| Esc | 退出全屏 |

搜索框等文本输入框聚焦时，快捷键自动失效，不会误触发。

## 使用方法

### 直接运行（推荐用户）

1. 解压发布包（archview.exe + libmpv-2.dll + config.json），双击 `archview.exe`。
2. 点击左上角 **Open Archive…** 选择压缩包，或直接把压缩包文件拖入窗口。
3. 左侧标签页切换 **视频 / 图片**，顶部搜索框可过滤文件名；点击文件即开始播放。
4. 右侧播放区底部为进度条，拖动到目标位置松手即跳转。

> 首次启动若 exe 同目录没有 config.json，会自动生成一份默认配置。

### 从源码构建

环境要求：Rust（MSVC toolchain）。

```bat
git clone <本仓库地址>
cd archview
cargo build --release
```

### 获取 libmpv-2.dll

Archview 的视频播放基于 libmpv，**仓库不含该 dll**（约 120MB，超出 GitHub 单文件限制）。获取方式任选其一：

1. **推荐**：下载 [shinchiro 的 mpv 构建产物](https://github.com/shinchiro/mpv-winbuild-cmake/releases)（`mpv-x86_64-*.7z`），解压后取其中的 `libmpv-2.dll`
2. 或使用 [mpv 官方构建](https://mpv.io/installation/) 中附带的 dll

放置位置：`deps\mpv-dev\libmpv-2.dll`（开发期由 build.rs 自动复制到 target 目录），或直接与 exe 同目录。

### 打 release 包

```bat
package.bat
```

产物：`dist\`（exe + libmpv-2.dll + config.json）及 `dist.zip`。可用环境变量覆盖路径（不硬编码）：

- `ARCHVIEW_MPV_DIR`：libmpv-2.dll 所在目录（默认 `deps\mpv-dev`）
- `ARCHVIEW_DIST`：输出目录（默认 `dist`）

## 测试方法

### 运行全部测试

```bat
run_tests.bat
```

或直接：

```bat
cargo test -- --nocapture
```

### 测试覆盖说明

| 测试 | 验证内容 |
|---|---|
| VFS 单元测试 | ZIP / 7Z / RAR 各自的列表、读取、seek 正确性；7Z 测试包由 sevenz-rust 纯代码构造，无外部依赖 |
| e2e 流式播放 | headless mpv（vo=null ao=null cache=no）经 zvf:// 协议真实播放 deflate ZIP 内的 mp4，验证前向 / 后向 seek，以及 EOF 后切换视频能正常续播（keep-open 暂停粘滞回归） |
| e2e 解压播放 | 构造 7Z 包 → 后台解压 → headless mpv 播放本地文件全链路 |
| flag 属性循环 | pause / mute 属性经 flag 格式读 → 翻转 → 写 → 读，覆盖空格暂停、静音按钮的行为路径 |

e2e 测试不需要人工看画面：mpv 以无渲染模式运行，通过 `time-pos` / `pause` / `eof-reached` 等属性断言播放状态，`cache=no` 强制 seek 请求打到 VFS 层，能真实暴露流实现缺陷。

RAR 测试需要本机装有 WinRAR（探测环境变量 `ARCHVIEW_TEST_RAR` 或 Program Files 常见路径，找不到自动跳过，不算失败）。

## 配置文件 config.json

启动时按「exe 同目录 → 当前工作目录」顺序查找；不存在则写入默认值。所有路径均可为绝对路径，`"auto"` 表示自动探测。

```json
{
  "cache": {
    "dll_dir": "auto",
    "dir": "auto",
    "max_size_mb": 8192,
    "chunk_size_mb": 4,
    "chunk_cache_blocks": 64,
    "clean_on_exit": false,
    "clean_on_switch": false
  },
  "media": {
    "video_extensions": ["mp4", "mkv", "avi", "mov", "flv", "ts", "webm", "wmv", "m4v"],
    "image_extensions": ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"]
  },
  "player": {
    "hwdec": "auto",
    "cache_secs": 30,
    "demuxer_max_mb": 512,
    "volume": 100
  },
  "ui": {
    "font_path": "auto",
    "slideshow_secs": 1.0,
    "wheel_switch_secs": 0.3,
    "ask_next_archive": true
  },
  "password_codepage": "gbk"
}
```

| 配置项 | 说明 |
|---|---|
| `cache.dll_dir` | libmpv-2.dll 搜索目录，`auto` = exe 同目录 |
| `cache.dir` | 7Z/RAR 解压缓存目录，`auto` = `%TEMP%\archview_cache`；可指向 RAM Disk 提速（见下文"进阶：RAM Disk 缓存"） |
| `cache.max_size_mb` | 缓存总量上限，超出按 LRU（最久未访问）驱逐 |
| `cache.clean_on_exit` | 退出时是否自动清空缓存（默认 false） |
| `cache.clean_on_switch` | 切换压缩包时是否清理上一个包的缓存（默认 false，保留以便切回时命中） |
| `media.*_extensions` | 视频 / 图片的文件扩展名识别列表 |
| `player.hwdec` | 硬件解码（`auto` / `no` / `d3d11va` 等 mpv 取值） |
| `ui.font_path` | CJK 字体路径，`auto` 依次尝试系统 msyh / simhei / simsun |
| `ui.slideshow_secs` | 图片自动播放（PPT 式翻页）间隔秒数 |
| `ui.wheel_switch_secs` | 视频区滚轮切换媒体的最小间隔秒数 |
| `ui.ask_next_archive` | 翻到本包最后一项再往后导航时，是否弹窗选择打开同目录其它压缩包（默认 true；列表默认选中循环意义上的下一个，末包绕回第一个；目录内没有其它包时仅顶栏提示） |
| `password_codepage` | 加密包密码候选代码页（老工具按系统 ANSI 编码密码），如 `gbk` / `big5` / `shift_jis` / `euc-kr`；`none` 只试 UTF-8 |

### 路径写法（cache.dir 等所有路径项）

标准 JSON 字符串，双引号包裹。Windows 下正斜杠、反斜杠都认，但反斜杠必须**双写**（JSON 转义）：

```json
"dir": "D:/my_cache"       // 推荐：正斜杠，无需转义
"dir": "D:\\my_cache"      // 反斜杠必须双写，单个 \ 会解析报错
"dir": "auto"              // 特殊值：自动探测
```

常见错误：单引号 `'D:/cache'` 不合法（JSON 只认双引号）；`"C:\cache"` 因 `\c` 非法转义导致整个 config.json 解析失败。相对路径相对的是 exe 启动时的工作目录，建议写绝对路径。目录不存在会自动创建。

### 进阶：RAM Disk 缓存（可选推荐）

7Z/RAR 走"后台解压到磁盘缓存再播放"的慢路径，缓存落在机械盘上时首次播放等待明显。把 `cache.dir` 指向 RAM Disk 可以显著加速，且**断电/重启即自动清空**，无需维护：

1. 用 [ImDisk Toolkit](http://www.ltr-data.se/opencode.htm/)、OSFMount 或 [Primo Ramdisk](https://www.romexsoft.com/zh/primo-ramdisk/) 等工具划出一块 RAM Disk（建议 2~4GB，盘符如 `R:`）
2. 修改 config.json：

```json
{
  "cache": {
    "dir": "R:/archview_cache",
    "max_size_mb": 3072,
    "clean_on_exit": false,
    "clean_on_switch": false
  }
}
```

3. `max_size_mb` 建议设为 RAM Disk 容量的 70%~80%，留出余量；超出上限时按 LRU 自动驱逐最久未访问的缓存

> RAM Disk 容量有限：单条目超过剩余空间时（如蓝光原盘），解压前会预检磁盘余量并**直接报错提示**（不再发起注定失败的解压）——此时需手动把 `cache.dir` 改回磁盘目录并重启程序。`clean_on_exit` / `clean_on_switch` 保持 false 即可：RAM Disk 生命周期与系统一致，重启自动归零。

### 缓存清理时机

1. **手动**：顶栏「清理缓存」按钮，显示释放空间；「缓存目录」按钮用资源管理器打开。
2. **切换压缩包**：`clean_on_switch` 为 true 时删除上一个包的解压缓存（按 archive_id 前缀匹配）。
3. **退出程序**：`clean_on_exit` 为 true 时清空整个缓存目录。
4. **容量上限**：超过 `max_size_mb` 时 LRU 驱逐最旧的文件。
5. **启动**：清理上次中断解压留下的 `.part*` 半截文件。

缓存按压缩包路径稳定命名：同一包 A→B→A 切回（或程序重启后重开）缓存直接命中，无需重新解压。

## 软件设计

### 总体架构

```
┌─────────────────────────────────────────────┐
│ UI 层 (ui.rs, egui)                          │
│  文件列表(标签页/搜索) · 播放区布局 · 快捷键    │
├──────────────────┬──────────────────────────┤
│ PlayerService     │ 后台解压线程 (ExtractJob)  │
│ (player/mod.rs)   │ 进度消息 → UI 轮询         │
│ mpv 命令封装       │                          │
├──────────────────┴──────────────────────────┤
│ VFS 抽象层 (vfs/)                            │
│  ArchiveVFS trait + ZIP/7Z/RAR 三种实现       │
├─────────────────────────────────────────────┤
│ mpv stream_cb (zvf://) │ cache.rs (LRU缓存)  │
└─────────────────────────────────────────────┘
```

### 模块划分

| 模块 | 职责 |
|---|---|
| `src/main.rs` | 入口：日志初始化、配置加载、libmpv 动态库初始化、启动 eframe |
| `src/ui.rs` | 全部 UI：顶栏（打开/清理缓存）、文件列表标签页、播放区（mpv 子窗口嵌入 + 底部全宽进度条）、控制栏、快捷键分发、全屏管理、后台解压任务轮询 |
| `src/config.rs` | config.json 的加载 / 默认值生成 / 扩展名与路径探测（唯一配置来源） |
| `src/cache.rs` | 缓存目录管理：LRU 驱逐、部分解压清理、全量 / 按包清理 |
| `src/player/mod.rs` | mpv 播放服务：加载、seek、暂停、音量等命令封装；headless e2e 测试 |
| `src/player/mpv_ffi.rs` | mpv 最小 FFI 绑定：libloading 运行时动态加载 libmpv-2.dll |
| `src/player/stream_cb.rs` | zvf:// 自定义流协议：mpv 回调 ↔ VirtualFile 的桥接与注册表 |
| `src/vfs/mod.rs` | `ArchiveVFS` / `VirtualFile` trait 定义 + 按扩展名分发打开 |
| `src/vfs/zip_vfs.rs` | ZIP 实现：自研 deflate 流式 seek（前向跳读、后向重开） |
| `src/vfs/sevenz_vfs.rs` | 7Z 实现：sevenz-rust 纯 Rust 解压（无外部 exe 依赖） |
| `src/vfs/rar_vfs.rs` | RAR 实现：unrar crate（内嵌编译 UnRAR 源码），分卷支持 |

### 核心设计决策

**1. VFS 抽象层。** 所有压缩格式实现统一接口：

```rust
trait ArchiveVFS {
    fn list(&self) -> &[VirtualEntry];        // 媒体文件清单（含子目录平铺）
    fn open_entry(&self, idx: usize) -> Result<Box<dyn VirtualFile>>;
    fn access_mode(&self) -> AccessMode;      // Stream(流式) / Extract(需解压)
    fn extract_entry(&self, idx: usize, dest: &Path) -> Result<u64>;
}
```

播放器和缓存层只面向 trait 编程，新增格式（如分卷 ZIP）只需加一个实现。

**2. 双路径播放。**

- **ZIP 快路径**：mp4 demuxer 需要随机读（读头部元数据 + seek 到尾部索引），ZIP 的 deflate 流可以按块重解压实现廉价前向 seek，因此走 `zvf://` 流式协议直接播压缩包内数据，零解压等待。
- **7Z/RAR 慢路径**：这两种格式多为 solid 压缩（中间任何字节都依赖前面全部数据），随机 seek 代价极高，所以走后台解压：UI 发起 ExtractJob → 子线程解压到缓存目录（.part 临时名 + rename 原子化）→ 完成后 mpv 播本地文件。解压期间界面保持响应并显示进度。

**3. mpv stream_cb 桥接。** 通过 `mpv_stream_cb_add_ro` 注册 `zvf://<archive_id>/<entry_idx>` 协议，read/seek/size/close 四个回调直接对接 `VirtualFile`。关键约束：

- 回调在 mpv 内部线程执行，**回调内禁止调用任何 mpv API**（死锁），跨线程状态经全局 REGISTRY 传递。
- cookie 使用「双重装箱」：`Box<dyn VirtualFile>` 是胖指针，直接 as `*mut c_void` 会丢 vtable 导致崩溃，须先 `Box::new(vfile)` 再 `Box::into_raw`。

**4. libmpv 动态加载。** shinchiro 构建的 libmpv.dll.a 是 MinGW 导入库，MSVC 链接不了；因此用 libloading 运行时加载 dll，FFI 绑定按需手写（约 15 个符号），换 mpv 版本无需重新编译。

**5. 子窗口嵌入与坐标同步。** mpv 以 `wid=<hwnd>` 渲染进 egui 画的占位矩形对应的原生子窗口；每帧把 egui 逻辑坐标乘 pixels_per_point 换算成物理像素后 SetWindowPos。全屏即隐藏全部 egui 面板、子窗口随中央面板铺满，退出时恢复。

**6. 加密包密码链路。** 压缩包里的密码只是一串字节，各格式编码约定不一（7z=UTF-16LE、RAR5/跨平台工具=UTF-8、Windows 老工具创建的 zip/RAR4=系统 ANSI 代码页）。解压时生成 `UTF-8 + 配置代码页（默认 GBK）` 两个候选字节序列依次尝试（config 项 `password_codepage`）；密码仅存内存，会话内按包路径记忆（`HashMap<PathBuf, String>`），切换压缩包再切回无需重输，退出即丢。

**7. 稳定缓存命名与并发解压防护。** 缓存文件名中的 archive_id 取压缩包路径哈希（带注册表内防碰撞探测），同一路径的包切走再切回、甚至程序重启后重开，缓存均可命中；解压走"临时文件（`part{pid}_{seq}` 防多实例/同进程并发踩踏）+ 原子改名"，同一任务在跑时去重不再起新线程。

**8. 已知坑位备忘。**

- `keep-open=always` 在 EOF 时置 `pause=yes` 且 `loadfile` 不清除——切换视频前必须显式恢复 `pause=no`，否则新视频停在第一帧。
- `pause` / `mute` / `eof-reached` 是 flag(bool) 属性，必须用 `MPV_FORMAT_FLAG` 读；用 INT64 读恒失败。
- egui 0.36 无 `allocate_ui_at_rect`，用 `allocate_rect` 顺序布局实现「视频矩形 + 底部进度条」。

### 线程模型

- **UI 线程**：egui 渲染、快捷键、mpv 命令（mpv handle 线程安全，但仅 UI 线程持有）。
- **mpv 内部线程**：demux/decode，经 stream_cb 回调读压缩包数据，与 UI 仅通过 REGISTRY（Arc<dyn VirtualFile>）共享。
- **解压线程**：每个 ExtractJob 一条线程，通过 mpsc channel 报告进度/结果，UI 每帧轮询；带代际号（gen），用户快速连续切换时旧任务结果直接丢弃。

## 已知限制与规划

- 分卷 ZIP：暂不支持（分卷 RAR 已支持）。
- 密码候选代码页只试 UTF-8 + 一个配置编码（默认 GBK/简中）；包若在日文等其它代码页系统上创建，需把 `password_codepage` 改为对应编码（如 `shift_jis`）。
- 视频缩略图、播放进度记忆：规划中。

更多实现细节见 `ARCHITECTURE.md`（架构设计：VFS 抽象层、双路径播放、密码链路、缓存策略）。

## 开源协议

[MIT](LICENSE)。注意：本程序运行时动态加载的 libmpv 以其自身的开源协议（GPL/LGPL）分发，dll 需自行获取（见上文"获取 libmpv-2.dll"）。
