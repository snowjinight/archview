# 压缩包媒体预览播放器 — 架构设计

> 目标：打开一个压缩包（RAR / 7Z / ZIP），浏览其中分散在各子目录的视频与图片文件，
> 视频可直接播放（支持拖动进度条 seek），图片可预览（类 HoneyView 体验），无需完整解压。
>
> **配套文档**：[DEVLOG.md](DEVLOG.md)——修复记录（现象→根因→修复→教训）、踩坑点、
> 排查手册等细节篇。本文只保留宏观内容（机制设计、技术决策、数据流、不变量），
> 引用约定 `DEVLOG §N`。

---

## 0. 术语约定：上游 / 下游

全文用一对绝对方位词描述方向，避免"向前/向后"的歧义（UI 视角"前"=列表上方=下标更小；
解码流视角"前"=沿流推进=下标更大，两义相反，已弃用）：

- **下游** = 解码流推进方向 = 条目下标递增 = 列表向下 = 文件尾 = 播放时间更晚；
- **上游** = 逆解码流方向 = 条目下标递减 = 列表向上 = 文件头 = 播放时间更早。

相对目标的位置可用"前置条目（上游）/ 后续条目（下游）"表述。旧词 → 新词对照：

| 旧词（已弃用） | 新词 | 说明 |
|---|---|---|
| 向后预取 / `prefetch_mb` | 下游预取 / `prefetch_downstream_mb` | 顺解码流预取后续条目（翻页即命中） |
| 向前预取 / `prefetch_prev_mb` | 上游预取 / `prefetch_upstream_mb` | 定位目标必然顺带解掉的前置条目写盘（回看即命中） |
| 前向 seek | 下游 seek | 流内偏移增大，续解丢弃输出 |
| 后向 seek | 上游 seek | 流内偏移减小，重开 entry 丢弃到目标偏移 |
| 锚点后方 / 后退浏览 | 锚点上游 / 回看 | keep 分支判定（见 11.4 / DEVLOG §4） |
| 锚点前方 / 前移点击 | 锚点下游 / 向下游点击 | 重定向滑窗触发条件 |

锚点（focus）**只向下游推进不回退**；旧配置键名 `prefetch_mb` / `prefetch_prev_mb`
经 serde alias 仍可读取（config.rs `extract_prefetch_legacy_keys` 测试钉死）。

---

## 1. 需求概述

| 需求 | 说明 |
|---|---|
| 压缩包格式 | ZIP、7Z、RAR（含 RAR4/RAR5、分卷） |
| 媒体类型 | 视频（播放）、图片（预览），文件分散在压缩包任意子目录 |
| 播放能力 | 内嵌播放、拖动进度条 seek、暂停/音量等常规控制 |
| 图片预览 | 缩放/平移/适应窗口、上一张/下一张、动图(GIF/动画 webp 按帧逐帧播) |
| 加密包 | 支持密码输入（不落盘） |
| 配置 | 所有可变项集中在单一 JSON 配置文件，与代码解耦 |

---

## 2. 关键技术决策

### 2.1 语言与主要依赖（Rust）

| 能力 | Crate | 说明 |
|---|---|---|
| UI | `egui` / `eframe` | 纯 Rust，即时模式，单二进制分发 |
| ZIP 读取 | `zip` | 纯 Rust，支持按 entry 随机访问 |
| 7Z 读取 | `sevenz-rust` | 纯 Rust；`BlockDecoder` 支持固态 folder 直达解压（见 11.1） |
| RAR 读取 | `unrar` | 绑定官方 UnRAR 库（Windows 打包 UnRAR.dll），支持 RAR4/5、分卷 |
| 视频播放 | 自研 `mpv_ffi` | libloading 运行时动态加载 libmpv-2.dll + 手写最小 FFI 绑定（约 20 符号：核心 + stream_cb + render API）。不用 libmpv-rs：shinchiro 构建的 libmpv.dll.a 是 MinGW 导入库，MSVC 链接不了 |
| 图片渲染 | `image` crate | 静态图与 GIF/动画 webp 动图走 egui 纹理直显（13.4）：两档纹理按屏幕密度适配（基础 ×ppp / 深放大 full ×ppp×2）/全帧预解码/LRU 预取；mpv 收缩为纯视频 + 解码失败回退 |
| GL 后端 | `eframe` (glow) | 渲染器显式指定 glow（0.34 起 eframe 默认 wgpu，glow 才能拿到 `GL` 上下文喂给 mpv render API） |
| 配置 | `serde` + `toml` | config.toml 带中文注释的默认模板 + 防漂移测试 |

### 2.2 播放方案：内嵌 libmpv（而非外部播放器 / 裸 FFmpeg）

- **外部播放器**：只接受文件路径，压缩包内文件无法直接喂入；临时文件/命名管道方案体验割裂，弃用。
- **裸 FFmpeg（ffmpeg-next）**：需自研 demuxer/解码/渲染/音画同步/seek，工作量等于自研播放器，弃用。
- **libmpv（选定）**：mpv 即成熟的 FFmpeg 前端。关键能力 **stream_cb**：注册自定义协议（如 `zvf://`），
  mpv 播放时通过 open/read/seek/close 回调向宿主要数据 —— 压缩包内数据如普通流般喂入，
  解码、渲染、音画同步、进度条 seek 全部由 mpv 提供。

### 2.3 解压策略：按压缩包能力分级，非二选一

核心判据：**该 entry 是否支持廉价随机访问**（播放器 demuxer 需要来回 seek 建 index）。

```
打开压缩包
  ├─ ZIP ────────────────── entry 独立压缩，seek 便宜 ──→ 流式按需读取（快路径）
  ├─ RAR 非固态 / 7z 全独立 folder ── 每文件独立压缩/独立 folder，定位 = 廉价头跳转
  │                / folder 直达 ──→ 单文件按需解压（批 = 1，只解点击项，无滑窗/预取；
  │                unrar 无流式读 API，不走 mpv stream_cb）
  └─ 7Z 多文件同 folder / RAR 固态(solid) ── seek 需从固态块起点重解 ──→ 后台解压会话
                                                          + 滑窗预取（慢路径）
                                                          解压至 N MB 即起播，边解边播
```

> 注意：solid 判定读格式头——RAR 主头 ROADF_SOLID 标志（`rar a -s` 设置；rar **默认非固态**）；
> 7z 无全局 solid 标志，按 folder 结构判定（**任一 folder 含 ≥2 条目 = solid**；`7z a -ms=off`
> 产出的全独立 folder 包走单文件按需）——网上流传的合集包两种都有，两条路径都是必选实现，
> 不是优化项。
> 本地磁盘解压速度远超播放码率，起播延迟通常 1~3 秒。

---

## 3. 整体架构

```
┌─────────────────────────────────────────────────────┐
│  UI 层 (egui)                                       │
│  左侧：文件列表（视频/图片标签页）+ 搜索            │
│  右侧：媒体区 —— 视频 mpv render API / 图片 egui 纹理 │
│         （图片含动图走纹理直显 13.4，mpv 只管视频； │
│           视频 PaintCallback 内 blit 到 egui 画布）  │
│  顶栏：打开/缓存管理；状态行：文件名/进度           │
└──────────────────┬──────────────────────────────────┘
                   │ 命令/事件
┌──────────────────▼──────────────────────────────────┐
│  应用核心 (状态机：浏览/打开中/播放/解压中)        │
└────┬───────────────────┬────────────────────────────┘
     │                   │
┌────▼───────────┐  ┌────▼──────────────────────┐
│  播放服务       │  │  VarTidy 整理器（CLI）     │
│  mpv_ffi       │  │  分类 + 去重 + 批量抽取    │
│  stream_cb     │  │  → 直写 var 仓库（只增）   │
└────┬───────────┘  └────┬──────────────────────┘
     │                   │ open_entry / extract_batch
     │                   │ （播放侧解压走 extract_session，见 11.4）
     │ Read + Seek       │
┌────▼───────────────────▼─────────────────────────────┐
│  VFS 层（核心抽象，播放与整理共用）                 │
│  trait ArchiveVFS { list(); open_entry() }           │
│  trait VirtualFile: Read + Seek { size() }           │
│  ├─ ZipVFS    （随机访问）                           │
│  ├─ SevenzVFS （BlockDecoder folder 直达；folder 结构│
│  │             探测 solid/非 solid 路由）            │
│  ├─ RarVFS    （探测固态；分卷合并处理）             │
│  └─ DirVFS    （目录媒体集合，见 4.2；FastSeek 直通）│
└────┬─────────────────────────────────────────────────┘
     │ 固态回退：后台解压
┌────▼──────────────────┐   ┌──────────────────────────┐
│  缓存管理器            │   │  配置服务                │
│  · 解压文件缓存（慢路径）│   │  config.toml 单文件：   │
│  · 解码块 LRU（快路径） │   │  缓存/扩展名/播放器参数/ │
│  · LRU 淘汰 + 上限控制  │   │  看图参数/VarTidy       │
└───────────────────────┘   └──────────────────────────┘
```

**架构不变量**：播放器与 VarTidy 整理器是 VFS 层的两个并列消费者，互不感知。
未来扩展（音频播放、文本预览等）只需新增消费者，VFS 不动 —— 这是本设计的核心扩展点。

**线程模型**（全进程 5 类线程，边界即数据流边界）：

| 线程 | 职责 | 与 UI 线程的通信 |
|---|---|---|
| UI 线程 | egui 渲染、快捷键、mpv 命令下发、全部状态机 | —（mpv handle 线程安全，但仅 UI 线程持有） |
| 打开线程（OpenJob） | 每包一条：解析压缩包头 + 列目录 + 密码探测 | mpsc channel 交 UI 应用；带代际号（gen），快速连续切换时旧结果丢弃 |
| mpv 事件泵线程 | 常驻：`wait_event(0.2)` 消费 mpv 事件（EOF / 文件切换 / 属性订阅推送）写入原子缓存 | 原子变量——UI 帧内零锁读（见 13.2 订阅缓存） |
| mpv 内部线程 | demux / decode / playloop，经 stream_cb 回调读压缩包数据 | 仅通过 REGISTRY（`Arc<dyn VirtualFile>`）共享 |
| 解压会话线程 | 每包一条持久解码会话（targets / focus 经 `Arc<Mutex<SessionState>>` 共享）；跳转 = 重定向目标、永不重启解码 | mpsc 报告进度 / 结果 / FileDone，UI 每帧轮询（见 11.4） |

**模块清单**（`src/`，职责以各文件头注释为准）：

| 模块 | 职责 |
|---|---|
| `main.rs` | 入口：日志初始化、配置加载、libmpv 动态库加载、启动 eframe |
| `config.rs` | config.toml 加载 + 默认值生成（唯一配置来源，§7） |
| `cache.rs` | 缓存目录管理：LRU 驱逐（按 mtime）、`.part` 半截文件清理、全量 / 按包清理 |
| `ui/mod.rs` | UI 帧循环主体：文件列表（标签页/搜索/状态行）+ 播放区 + 控制栏 + 滚轮模式感知 |
| `ui/panels.rs` | 各面板绘制：顶栏、虚拟化文件列表、媒体区（视频 blit / 解压覆盖层 / 图片纹理）、控制栏与进度条 |
| `ui/nav.rs` | 快捷键与导航：全屏、mpv 风格按键、上/下一个、幻灯片自动翻页 |
| `ui/extract.rs` | 播放与后台解压链路：mpv 播放器管理、批次计算与磁盘预检、solid 会话任务提交/轮询、密码重放与已解压文件装载 |
| `ui/open.rs` | 打开链路：文件/目录对话框、拖放入口、密码分支、后台打开任务轮询与视图重建 |
| `ui/dialogs.rs` | 弹窗：密码输入（加密头/内容加密两阶段）与「下一个压缩包」选择 |
| `ui/tests.rs` | UI 层单元测试（滑动窗口冒烟等，DEVLOG §5.2） |
| `imgview.rs` | 图片纹理直显服务 ImageService：后台解码、两档纹理、LRU、动图时钟（13.4） |
| `player/mod.rs` | mpv 播放服务：命令封装、常驻事件泵、属性订阅缓存、render context 管理；headless e2e 测试 |
| `player/mpv_ffi.rs` | mpv 最小 FFI 绑定：libloading 运行时动态加载 libmpv-2.dll（核心 + stream_cb + render API 符号） |
| `player/stream_cb.rs` | zvf:// 自定义流协议：mpv 回调 ↔ VirtualFile 桥接与 REGISTRY（见 §4 约束） |
| `vfs/mod.rs` | `ArchiveVFS` / `VirtualFile` trait 定义、按扩展名分发打开、密码探测链 |
| `vfs/zip_vfs.rs` | ZIP：自研 deflate 流式 seek（下游跳读、上游重开）+ CD 原始字节文件名解码（4.1） |
| `vfs/sevenz_vfs.rs` | 7Z：BlockDecoder folder 直达（11.1）+ 解码会话（11.4） |
| `vfs/rar_vfs.rs` | RAR：unrar crate（内嵌编译 UnRAR 源码），分卷支持 |
| `vfs/dir_vfs.rs` | 目录媒体集合 DirVFS（4.2） |
| `vartidy/mod.rs` | VarTidy CLI：输入展开 → 规划（分类 + 两级去重）→ 批量抽取直写仓库 → 报告；`tidy` 参数解析（§12） |
| `vartidy/classify.rs` | var 文件名分类规则（class_var.py 移植：作者.内容.版本.var 四段校验） |

---

## 4. 核心抽象：VFS 层

```rust
/// 统一三种压缩包格式的读取接口
trait ArchiveVFS {
    fn open(path: &Path, password: Option<&str>) -> Result<Self>;
    /// 递归列出所有 entry（扁平 + 路径信息）
    fn list(&self) -> Vec<VirtualEntry>;
    /// 打开单个文件，返回可读可 seek 的虚拟文件
    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>>;
    /// 决定播放策略的关键能力声明
    fn access_mode(&self) -> AccessMode; // FastSeek | Solid(需缓存回退)
}

/// mpv stream_cb 回调直接对接此 trait；图片查看器同样消费此 trait
trait VirtualFile: Read + Seek {
    fn size(&self) -> u64;
}
```

播放侧注册 mpv 自定义协议（`zvf://archive_idx/entry_idx`），open/read/seek 三个回调
直接转发给 `VirtualFile` —— **mpv 的 seek 就是 VirtualFile 的 seek**，快/慢两条通路对播放器完全透明。
两个关键约束（stream_cb.rs）：

- 回调在 mpv 内部线程执行，**回调内禁止调用任何 mpv API**（死锁）；跨线程状态
  经全局 REGISTRY（`Arc<dyn VirtualFile>`）传递，回调只做纯转发；
- cookie **双重装箱**：`Box<dyn VirtualFile>` 是胖指针（数据指针 + vtable），直接
  `as *mut c_void` 会丢 vtable 导致回调内崩溃——须先 `Box::new(vfile)` 再
  `Box::into_raw`，close 回调按同原型回收。

### 4.1 zip 老包文件名编码（GBK 等）

ZIP 规范（APPNOTE）：文件名无 bit11 UTF-8 标志时按 CP437 解释。老工具（老 WinRAR/好压）
在简中 Windows 上实际按系统 ANSI（GBK）存名字字节——按规范解即乱码，且 zip crate 的
`index_for_name` 以原始字节为 key 查内部表，CP437 解出的字符串 UTF-8 字节必然与原始
GBK 字节失配，`open_entry` 报"entry 不存在"（列表能显示但点开必失败）。

解法（zip_vfs.rs）：`open()` 时顺序扫一遍 CD 收集原始名字节与 flags（一次顺序读，
无逐条 seek；条目数与 crate 不一致的重名折叠等边缘包整体放弃，回退 crate 旧行为），
按链解码：**UTF-8 标志置位或字节为合法 UTF-8 → UTF-8；否则配置码页（config `codepage`，
默认 GBK）；码页解出替换符 → lossy**。解码结果两处消费：`list` 的显示名 +
自建「显示名 → crate 下标」表（`open_entry`/`verify_password` 查找，替代按名字节
查 crate 内部表的 `index_for_name`）。密码与文件名共用同一 `codepage`——老工具
打包时两者同按系统 ANSI 编码，本就同源。VarTidy 同链受益：GBK 名 var 入库不再乱码。

**密码字节的编码约定**（与文件名同源问题）：各格式密码编码约定不一——
7z = UTF-16LE、RAR5 与跨平台工具打的包 = UTF-8、Windows 老工具创建的
zip / RAR4 = 系统 ANSI 代码页。探测时对每个候选密码生成「UTF-8 + 配置码页
（默认 GBK）」两个字节序列依次验证。密码仅存内存，会话内按包路径记忆
（`HashMap<PathBuf, String>`），切换压缩包再切回无需重输，退出即丢。

### 4.2 目录 = 媒体集合（DirVFS）

打开目录不再"寻找目录内的压缩包"，而是把目录本身当作媒体集合：**递归收集其中
图片/视频（按 config 扩展名过滤，压缩包等其它文件天然被排除），跳过压缩包不展开**。
一条规则覆盖全部入口——`initial_archive`（命令行/SendTo/拖到 exe）、拖放进窗口、
「打开目录」按钮，它们都汇聚到 `spawn_open` 的目录分支。混合多选（目录+压缩包同时
「发送到」）按参数顺序取**第一个有效项**打开，其余忽略——语义可预期优先。

实现（`dir_vfs.rs`）：`DirVFS` 实现 `ArchiveVFS`，`access_mode = FastSeek`——本地文件
随机访问 O(1)，与 ZipVFS 同走 `zvf://` 流式快路径，**解压会话/滑窗/缓存/密码探测等
慢路径机制零触碰**（条目全部 `encrypted=false`，密码探测零成本直通）。要点：

- `list()` 递归 walk，条目名 = root 下相对路径（`/` 分隔，与压缩包条目形态一致）；
  **按路径字典序排序**——目录集合没有"包内物理顺序"，`read_dir` 顺序又不可预期，
  字典序是 `ui.sort_files=false` 默认行为下最确定的顺序
- 符号链接 / junction 一律跳过（防环且语义可预期）；子目录不可读静默跳过
  （部分可读的树照常收集）；根目录不可读定性打开失败
- `open_entry()` = `std::fs::File` 套 `VirtualFile` 适配器（size 打开时缓存一次）
- 空集合（目录里没有媒体）打开成功，UI 在 Done 分支统一提示"没有可播放的视频/图片"

行为边界：目录集合模式下没有「同目录下一个压缩包」概念（末尾翻页提示改为一次性
说明）；「目录内连续看包」N 键语义仅在打开压缩包文件时保留。

---

## 5. Seek（拖动进度条）详细设计

### 5.1 结论

**完全支持。** 进度条拖动在 mpv 中即 `seek` 命令 → demuxer 将时间换算为字节偏移 →
触发 stream_cb 的 seek 回调 → `VirtualFile::seek()`。链路完整，差异仅在性能，
按下表分级处理。

### 5.2 各格式 seek 成本

| 格式/模式 | 下游 seek（向文件尾） | 上游 seek（向文件头） | 说明 |
|---|---|---|---|
| ZIP (store) | O(1) | O(1) | 直接偏移定位 |
| ZIP (deflate) | 便宜（跳读解压输出） | **贵**（从 entry 头重新 inflate） | 需解码块缓存缓解 |
| 7Z 非固态 | 便宜 | **贵**（LZMA 从 entry 头重解） | 同上 |
| RAR 非固态 | 便宜 | **贵** | 同上 |
| 任意格式固态（慢路径） | 需解压推进到目标点 | 同左 | seek = 解压任务重定向 |

> 多 GB 视频在 deflate/LZMA 下的上游 seek 若无缓存，可能需数秒 —— 因此以下缓解措施为**必选组件**。

### 5.3 缓解措施（四层）

1. **mpv demuxer 缓存**：调大 `demuxer-max-bytes` / `cache-secs`（config.toml 可配）。
   小幅回跳（刚看过的地方）命中缓存，零成本 —— 覆盖最常见的拖拽场景。
2. **解码块 LRU 缓存（快路径）**：解压后的数据按块（默认 4MB/块，可配）进 LRU；
   上游 seek 落在缓存内则 O(1)。播放为顺序读，"拖回刚才看过的地方"命中率极高。
3. **解压任务重定向（慢路径/固态）**：seek 超出已解压区域时，重定向解压会话的目标集
   （见 11.4），不终止解码流。UI 状态栏显示"跳转中…"。
4. **读取阻塞即缓冲**：stream_cb 的 read/seek 回调在 mpv 缓存线程上同步执行，
   阻塞是安全的（与网络流一致），mpv 自动显示 buffering 动画。

### 5.4 交互细节

- UI 进度条：观察 mpv `time-pos` 属性驱动进度显示；拖动松手后发 `seek` 命令（拖动中不连续发）。
- 慢 seek 期间：UI 显示 mpv buffering 状态；连续拖动产生的新 seek 取消前一个解压重定向任务。
- 快路径上游 seek 且缓存未命中：在 read 回调中同步重解压，期间 mpv 自然缓冲。

---

## 6. 图片预览模块

### 6.1 定位与实现决策

与视频播放并列的 VFS 消费者。早期与视频统一走 mpv loadfile（一条链路零成本复用
VFS/缓存/翻页）；**现已迁移至 image crate 解码 + egui 纹理直显**（13.4：翻页零延迟、
缩放平移、动图逐帧播放、预取对动图生效），mpv 降级为解码失败时的回退路径。

渲染层面：早期 mpv 以原生子窗口（wid 嵌入）盖在 egui 之上，形成双平面，引发一系列
窗口叠放绕路（见 11.3 历史、13 迁移动机）；**现迁至 mpv render API 后整个窗口只有
egui 一个平面**，视频/图片帧经 PaintCallback blit 进 egui 画布，覆盖层/弹窗/提示
天然盖在画面上，无需任何窗口操作。

### 6.2 数据流

```
选中图片 → ZipVFS 流式（zvf://）或固态包后台解压 → 缓存文件
        → image crate 解码 → egui 纹理直显（静态图 / GIF·动画 webp 动图逐帧，13.4）
        → 解码失败自动回退 mpv loadfile（旧链路保留为兜底；回退过的条目直接走 mpv）
        → PPT 式自动翻页（ui.slideshow_secs 间隔，空格/播放按钮开关）
```

### 6.3 功能与交互

| 功能 | 说明 |
|---|---|
| 翻页 | PgUp/PgDn、←/→ 切换上/下一个；滚轮在媒体区悬停时**模式感知**——视频或图片 fit 态 = 翻页（`ui.wheel_switch_secs` 节流），图片缩放态 = 锚光标缩放（缩小触底自动落 fit，13.4 交互）；文件列表区滚轮是正常滚动列表，不切换 |
| 动图 | GIF / 动画 webp 逐帧预解码 + egui 定时切帧（13.4 第 4 步）；webp 经 `has_animation()`（VP8X ANIM 位）先验分流——静态 webp 无 ANMF 块 `num_frames()==0`，盲走逐帧解出空集误报失败（实测踩坑），故静态走单帧 decode |
| 放大细节 | 两档纹理按屏幕密度（ppp）适配（13.4）：基础档 ×ppp 进 LRU（翻页即原生密度直出）；深放大（zoom×ppp>`image_full_threshold`，默认 2.0 即越过 244% 滚轮档（×2 密度采样比 ≥1 才有肉眼增益，195% 档实拍 A/B 无差））后台解 full 档 ×ppp×2（封顶 8192、无 mipmap 直采，`image_full_sharpen` 默认叠加 unsharp mask 锐化）无缝换画；100% = 1 纹素:1 物理像素（zoom=1/ppp）零重采样最锐点 |
| 自动播放 | PPT 式自动翻页，间隔 `ui.slideshow_secs` 可配 |
| 分类 | 视频/图片标签页按扩展名分栏（config.toml `media.*_extensions`） |

### 6.4 预留扩展：图片专属纹理浏览器（render API 已落地 ✅，落地方案见 13.4）

render API 迁移完成后，解锁"图片不走 mpv、直接解码进 egui 纹理"的专属看图路径：

| 能力 | 方案 | 相对 mpv 路径的收益 |
|---|---|---|
| 零延迟首帧 | image/zune-image 解码 → `ColorImage` → egui 纹理直显 | 消除 mpv loadfile 初始化 50~200ms（建 demuxer/GL 管线），翻页即显 |
| 缩放/平移 | 纹理 UV 变换（滚轮缩放锚定光标、拖拽平移），纯 egui 交互 | mpv `video-zoom/pan` 参数链路不再需要，交互跟手 |
| 解码器覆盖 | PSD/HEIC/AVIF 等专用格式接解码库走同纹理路径 | mpv 不支持的格式不再缺位 |
| 动图 | GIF 逐帧解码 → 定时更新纹理 | 与视频共用 PaintCallback rect，暂停/单帧步进自然实现 |

图片纹理路径与 mpv 路径并存（按格式/体积路由），VFS 层零改动——两者只是同一
VirtualFile 流的不同消费者。

---

## 7. 配置文件设计（config.toml，单文件集中）

```toml
codepage = "gbk"            # 非 UTF-8 字节兜底代码页（密码候选追加 + zip 老包文件名解码）
passwords = []              # 加密包密码候选列表（播放器与 VarTidy 公用，打开阶段自动探测）

[cache]   # dll 目录 / 解压缓存目录 / LRU 上限 / 解码块大小与块数 / 清理时机
[media]   # video_extensions / image_extensions
[player]  # hwdec / cache_secs / demuxer_max_mb / volume / muted
[ui]      # font_path / slideshow_secs / wheel_switch_secs / ask_next_archive / sort_files（自然序，默认关=包内顺序）/ extract_overlay_delay_ms / image_texture / image_cache_count / image_max_dim / image_full_threshold（full 触发阈值，默认 2.0）/ image_full_sharpen（full 锐化开关，默认开）——图片纹理直显，见 13.4
[extract] # prefetch_downstream_mb（下游预取上限）/ prefetch_upstream_mb（上游预取上限，见 11.1）
[vartidy] # repo_dir / report_dir（CLI 整理子命令）
```

> 约定：所有路径、阈值、扩展名列表、播放器参数均只在此文件配置，代码零硬编码。
> 格式选型：TOML（而非 JSON）——原生 `#` 注释 + 强制 UTF-8，默认配置带中文说明每个参数含义；
> 字符串必须双引号，无 YAML 裸标量陷阱（`yes/no` 变布尔、`#` 误注释）。
> 默认文件由手写模板生成（`config.rs` 的 `DEFAULT_TOML`），防漂移测试保证模板与代码默认值一致。

---

## 8. 风险点

| # | 风险 | 对策 |
|---|---|---|
| 1 | deflate/LZMA entry 上游 seek 慢 | 解码块 LRU（必选组件）+ mpv 缓存参数调大 |
| 2 | RAR5/7Z 固态 | 慢路径为必选实现；解压任务支持 seek 重定向；solid 按主头标志自动判定（非 solid RAR 走单文件按需） |
| 3 | 分卷压缩（.part1.rar / .7z.001） | unrar 原生支持 RAR 分卷；ZIP 分卷需自行拼接 —— 二期 |
| 4 | 加密 | 头加密阻止列目录 → 打开阶段自动探测密码（无密码 → config 列表），耗尽才弹密码框；密码仅内存传递不落盘 |
| 5 | libmpv 嵌入渲染 | 已迁移 mpv render API 纹理渲染（glow 后端 + PaintCallback，见 13）；原生子窗口双平面问题连根移除，剩余风险为 FFI 生命周期顺序（on_exit 释放 render ctx 先于 mpv terminate） |
| 6 | mpv dll 分发 | 仓库不含（超 GitHub 单文件限制），build.rs 从 `deps\mpv-dev` 复制，README 提供下载指引 |
| 7 | 大图解码阻塞 UI | 已消解：图片后台线程 image crate 解码 + mpsc 回 UI 线程上传纹理（13.4）；超 8192 病态图兜底降采样；解码失败自动回退 mpv |

---

## 9. 实施路线与状态

| 阶段 | 内容 | 交付物 | 状态 |
|---|---|---|---|
| Phase 1 (MVP) | egui + libmpv + ZipVFS + 列表/搜索 + config + seek 基础链路 + 解码块 LRU | ZIP 内视频端到端可播可拖 | ✅ |
| Phase 2 | SevenzVFS / RarVFS + 固态探测 + 后台解压通路 + LRU 文件缓存 + 大包性能专项（第 11 章） | 三格式全覆盖，56G 固态包秒级定位 | ✅ |
| Phase 3 | 图片查看（mpv 统一渲染 + PPT 自动翻页）+ 密码支持（多编码候选 + 会话记忆） | 图片/动图可看，加密包可开 | ✅ |
| Phase 4 | 分卷 RAR（已支持）、分卷 ZIP、视频缩略图、播放记忆 | 完整版 | 🔶 部分完成 |
| 追加 | VarTidy CLI 子命令（第 12 章）：var 资源包增量整理 | `archview.exe tidy` + 「发送到」 | ✅ |
| 追加 | 解压会话化（11.4）：extract_session 持久解码会话，跳转零取消 | 固态包随机跳看重定向不重起 | ✅ |
| 追加 | 渲染架构迁移（13）：wid 原生子窗口 → mpv render API + glow 后端 + PaintCallback blit，删除全部窗口叠放绕路代码 | egui 单平面，视频画面缩放质量随窗口实时重采样 | ✅ |
| 追加 | 播放流畅性专项（DEVLOG §3）：常驻事件泵 + 六属性 observe_property 订阅缓存（UI 帧路径零 get_property）+ vsync 主钟 | 打开/切换/seek 全程 UI 不冻结 | ✅ |
| 下一步 | 图片专属纹理浏览器（13.4）：图片不走 mpv，解码直进 egui 纹理；翻页零延迟 + 缩放平移 + 动图逐帧 + 两档高清纹理（×ppp/×ppp×2）+ 模式感知滚轮 | 纹理路径与 mpv 路径并存互兜底；mpv 收缩为纯视频 | 🔶 1-4 步已落地（含物理像素对齐修复与交互改版），5（PSD/HEIC/AVIF）按需 |

---

## 10. 技术选型清单（汇总）

```
ui          : egui / eframe（glow 后端，Renderer::Glow；wgpu 默认下须显式指定）
gl          : glow 0.17（eframe glow feature 同款；供 mpv render API 与 FBO blit 使用）
player      : 自研 mpv_ffi（libloading 动态加载 libmpv-2.dll，含 stream_cb + render API）
archive/zip : zip
archive/7z  : sevenz-rust (BlockDecoder 固态直达)
archive/rar : unrar (UnRAR.dll)
image       : image 0.25（jpeg/png/webp/gif/bmp/tiff/ico/tga 解码 → egui 纹理直显，见 13.4；动画 webp 经 has_animation 分流逐帧）
config      : serde + toml (config.toml)
logging     : tracing
sort        : alphanumeric-sort (file list natural order, optional mode for comic/image collections)
```

---

## 11. 性能优化与虚拟列表设计（大包实战：27G ZIP / 56G 7z / 万级条目）

> 本章只保留大包性能专项的**机制设计**（批量解压摊薄、上下游预取、会话化、虚拟化三层）。
> 修复叙事（folder 直达 10 分钟卡顿、ZIP 头偏移写错）与虚拟列表四坑、多帧测试方法论
> 见 **DEVLOG §1**。核心教训：**万级条目 + GB 级包，任何 O(N) 的每帧操作、任何"顺带"
> 的全量顺序解压，都会被放大成分钟级卡顿。**

### 11.1 解压链路优化

#### 7z folder 直达（最重要的一次修复）

- sevenz-rust 的 `SevenZReader::for_each_entries` 从 folder 0 顺序解压全部前置 folder
  （不利用 pack 流偏移）；改用 `BlockDecoder::new(folder_index, ...)` 直接 seek 到目标
  folder 的 pack 流偏移，**前置 folder 只 seek 不解压**——56G 固态包第 417 条目从
  10+ 分钟降到秒级。修复叙事与教训见 **DEVLOG §1.1**。
- 该机制是固态包一切路径（播放定位 / 会话化 / VarTidy 原地整理）的性能前提。

#### 批量解压摊薄固态包定位成本（extract_batch + prefetch）

- 固态 folder 内部不可 seek，定位目标必须顺序解压一段前置数据——这部分成本无法消除，
  但可以**摊薄**：一次顺序解压流顺带解出一批条目（目标 + 顺序后续，总量 ≤ `extract.prefetch_downstream_mb`，
  默认 512MB）。翻页时直接命中缓存，无需再等。
- trait 层新增 `extract_batch(targets, cancel, progress, file_done)`：ZIP 用默认实现逐条循环；
  7z/RAR 各自实现单遍多目标（RAR 单次 processing 遍历内 skip 非目标、extract 目标）。
- 取消机制：`Arc<AtomicBool>`。批外跳转的"取消重起"后被解压会话化取代（见 11.4），
  cancel 只剩一个用途：切换压缩包时终止解码线程（取消点打日志的坑见 DEVLOG §1.3）。

#### 上游预取（免费区）：随机跳看后往回翻也能命中

- 定位目标时，前置数据**必然被顺序解掉丢弃**（7z 是目标 folder 内从头解码到目标、
  solid RAR 是逐条 skip 消费前缀）——把这段"本来要丢的字节"改写入缓存，解码零增量，
  只增加磁盘写入。随机跳看场景下用户往回翻直接命中缓存，无需再付定位成本。
- 免费区边界由格式决定（trait `locate_start`，默认 None = 不适用）：solid 7z = 目标所在
  folder 首条目（跨 folder 只 seek 不解压，上游预取**不得超出 folder 边界**——配置
  512MB 但 folder 前缀不足时实取以边界为准）；solid RAR = 整个前缀（unrar 无 folder
  直达）；**非 solid RAR 与全独立 folder 的非 solid 7z 无免费区**（二者定位都是廉价
  跳转，上游预取等于白白解码，见下）。
- 预算独立封顶（`extract.prefetch_upstream_mb`，默认 512MB，0 = 关闭），与下游预取
  `prefetch_downstream_mb` 互不挤占；仅收当前列表可见条目（目录/搜索过滤外的文件用户点不到）。
  上下游两批去重防 file_done 下标错位（sort_files 开启时 view 序 ≠ 物理序，两批可能重叠）。
- 进度显示配合：前置文件先于目标落地，写文件阶段字节按目标尺寸封顶
  （否则进度条出现"300MB / 60MB"怪相）；完成提示改"相邻 N 个文件"。

#### ZIP 头解析

- 单次顺序读中央目录（CD）即完成列表（NAS 随机读 → 顺序读，万级条目分钟级 → 秒级）；
  ZIP64 支持：offset/size 超 32 位时的扩展字段解析。CD 逐字段偏移曾写错 6 处的修复叙事
  见 **DEVLOG §1.2**。

### 11.2 打开与列表：后台化 + 虚拟化

#### 后台打开（OpenJob + 代数号）

- 打开/列表解析全部移入后台线程，UI 显示"正在打开… x%"，不再冻结。
- **代数号（generation）防错位**：用户快速切换压缩包时，旧任务完成回调不得套用到新包上——
  每个任务携带递增 gen，主线程只接受 `gen == 当前` 的结果，旧的直接丢弃。

#### 行列表虚拟化（egui 万级条目不卡的关键）

**设计**（三层）：

1. **行数据缓存**：`list_rows: Vec<Row>`（idx + 文件名引用）只在打开/搜索/切标签时重建一次。
   早期版本每帧对万级条目做 clone + format + to_lowercase 过滤，UI 直接卡死——
   **即时模式 UI 不等于可以每帧重算业务数据**。
2. **虚拟化渲染**：`ScrollArea::show_viewport` 只渲染可视行 ± 3 行（overscan 防滚动露底）。
   用 `skip_ahead_auto_ids(start_row)` 保持 widget Id 稳定（否则每帧 Id 漂移，选中态/悬态错乱）。
   内容区总高度 = `set_height(总行数 × step)`，让滚动条比例正确。
3. **行高实测反馈**：首帧用估算行高（18px）渲染，实测首行真实高度后反馈修正——
   避免 DPI 缩放（125%/150%）下估算与实际不符导致的行间缝隙或重叠。

**踩坑点**（四坑：usize 下溢 / 行高不均 / scroll_to_rect 坐标系 / 每帧重算，
每条均有回归测试钉死）与**多帧测试方法论**（`ctx.run_ui(RawInput)` 手动喂事件
序列跑 100+ 帧复现跨帧时序 bug）见 **DEVLOG §1.4/§1.5**。

排序默认值的设计依据（实测 7-Zip 打包无视添加顺序强制按字典序写入）：
`ui.sort_files` 默认 false——包内顺序 = 固态包物理码流顺序，预取最优；
true = 自然序（数字段按数值 1/2/10）供漫画/图集场景。排序实现预生成小写键，
比较器内不做 to_lowercase 分配。

### 11.3 解压进度 UX：顶栏 → 媒体区覆盖层

- 顶栏一行放不下长文件名（截断、进度条看不见）→ 进度信息移到媒体区居中覆盖层：
  完整文件名（换行）+ 两阶段进度（跳过前置文件阶段 = Spinner + 已解压字节；
  写文件阶段 = 字节进度条）。
- **防闪烁宽限期**：正在播 A 时切到需解压的 B，超过 `ui.extract_overlay_delay_ms`（默认 500ms）
  仍未解完才停止绘制视频帧并显示覆盖层（render API 下整个窗口只有 egui 一个平面——
  该帧 PaintCallback 不提交画面，覆盖层即自然露出；历史 wid 方案时代须 `ShowWindow`
  隐藏 mpv 原生子窗口，因 mpv 子窗口永远盖在 egui 之上）；
  500ms 内解完则画面从未中断——快速路径零闪烁。这是"延迟隐藏"而非"最短显示时间"：
  后者会人为拖慢快速解压。
- 预取场景（目标 == 正在播放条目）永不遮挡画面，顶栏弱色短标记即可。
- 顶栏短标记"后台解压中…（剩余 N 项）"（N = `j.pending.len()`）**常显**——只要有
  解压任务就显示：keep 回看上游缓存时能看到原窗口剩余量，等待新文件时与媒体区
  覆盖层信息重复无妨（一个短数字，不构成干扰）。

### 11.4 解压会话化：extract_batch 整批快照 → 持久解码会话（extract_session）

> 背景：extract_batch 是「整批快照」模型——UI 启动任务时一次性给全目标清单，用户跳到
> 批外条目只能取消整个任务重起：解码线程销毁重建、7z 目标 folder 从头重解、解到一半的
> 前置数据全部白付。频繁随机跳看（固态包最痛的场景）下，每次跳转都重付一轮定位成本。

**设计**：UI 与解码线程之间改为**共享会话**——`SessionState { targets: HashMap<名字, (下标, 落盘路径)>, focus, alive, gen }`（`SharedSession = Arc<Mutex<..>>`，vfs/mod.rs）。解码线程循环消费会话直到空闲退出；UI 任何时刻持锁改写目标集，**跳转永不取消解码流**：

```
UI 侧（任意跳转）        解码线程（每轮规划）
targets.clear()          规划轮：targets 空？
+ 写入新批 + focus         ├─ 是 → 同锁置 alive=false → Ok(()) 空闲退出
同锁读 alive：             └─ 否 → 按目标分组规划，逐组解压
  true  → 复用线程/通道      落地一个目标 → 持锁 remove + 快照 → 锁外回调 file_done
  false → 置 true + spawn
```

**协议的原子性要求**（三处必须各自在**同一次持锁**内完成，防竞态）：

1. 线程侧「targets 判空 → 置 alive=false 退出」与取消检查同锁——否则 UI 正在加目标的
   窗口期线程误退出，新目标永远没人解；
2. UI 侧「写目标 + 读 alive + 需要时置 true」同锁——否则判定复用后线程恰巧空闲退出；
3. 落地条目「remove + 状态快照」同锁且 remove 先行——防同一文件重复落地。

另：file_done 回调必须在**锁外**调用（std::Mutex 不可重入）；回调内再锁 session 是合法且
必要的——动态追加目标就靠它（见测试 `session_dynamic_targets_chain`：回调内链式加目标，
单次调用解出全部四个）。

**7z 实现**（sevenz_vfs.rs）：外层规划轮把 targets 按所在 folder 分组（BTreeSet 保序），
**focus 所在 folder 抢道排到 order[0]**——用户正在等的最先落地；逐 folder 仍用 BlockDecoder
直达（11.1 的 folder 直达不变）。每解一个条目持锁取三元组快照 `(是否目标, focus 漂移, folder 已清)`：
**漂移**（focus 移到了别的 folder 且仍在 targets 中）→ 立即中断当前 folder，外层重排后从新
focus folder 继续；被中断 folder 在后续轮次从头重解自愈（非目标条目只解码不落盘，重解成本 =
folder 前缀，远小于跨包重起）。无进展保险：一整轮零落地且未发生中断 → 对比 `gen`（会话
重定向代数，UI 每次重写 targets 同锁自增；解码线程每轮规划起始快照 round_gen）——gen 已变
说明期间 UI 换过目标，本轮零落地属正常，重新规划续跑（如重定向到已落地缓存项后各 folder
均已解净的收尾轮）；gen 未变才是真无进展 → `Err(Archive)`（防脏目标让线程空转，UI 据此
报错并回收）。

**RAR 实现**（rar_vfs.rs）：单次 processing 遍历内动态消费 targets（skip 非目标、extract
目标）；目标在游标上游（upstream）→ `PassFlow::Restart` 重启遍历——unrar 无 folder 直达，
重启要重付前缀，但会话仍免掉线程/通道重建与 UI 侧任务状态错乱。候选轮/Restart 双层循环
（重启上限 64）+ 连续两轮零落地的无进展保险。

**UI 统一重定向路径 = 滑动窗口**（ui.rs `start_extract`）：缓存命中/批内切换/批外跳转三场景
共用同一条路径——`compute_batch` 以 focus 为中心重算会话窗口（目标 + 下游预取 prefetch_downstream_mb +
上游免费区 prefetch_upstream_mb；已落地条目一律出窗，含目标本身——缓存命中时窗口纯做预取），
持锁替换 targets + focus + gen 自增。窗口是**全量重算**而非增量平移：每次目标切换都按磁盘
落地状态重算（落地即出窗、预算内补新），因此既会随连续观看向下游滑动，也会在跳回未落地处时整体
回移到该处——窗口语义 = 「用户接下来可能看的、还没解好的工作集」。**窗口随用户滑动**：连续
观看时每次切换都把窗口对齐当前位置，跨过窗口尾界无感续解（不必等整批重解）。

**锚点只向下游推进不回退（keep 分支）**：focus 的语义是**解码抢道锚点**（上次重定向写入的目标下标），
仅解码线程消费，与 UI 播放高亮（`App::playing`）零耦合。缓存命中且点击项在锚点**上游**
（view 序比较，sort 开启时也正确）→ keep：直接播放、只切 UI 侧 job 字段，**完全不碰会话**
（targets/focus/gen/pending/cancel 全不动）——等待 80+ 解压时回看 1-30 不会把窗口拽回去、
后台解压不受干扰；点完 10 再点 11 也全是 keep（连续回看缓存项绝不触发窗口重建）。只有
**向下游点击**（点击项在锚点下游或未落地）才重定向滑窗——既保住「边看边解」的窗口向下游推进，
又不让回看撕毁后台工作集。任一端不在当前 view（被过滤）时 upstream 判否，退回重定向路径
（安全默认）。keep 后 job.entry_idx 已是播放项：FileDone 不切画面、Done 兜底补播条件
（playing != entry_idx）不成立，画面稳定停在回看项。

| 场景 | 差异点（窗口重算 + 重定向共用） |
|---|---|
| 缓存命中（锚点上游） | keep：直接播放，会话纹丝不动（不滑窗、不自增 gen、不重建 pending） |
| 缓存命中（锚点下游/同位） | 直接播 + 窗口滑动；会话已死则照常 spawn 预取会话（围绕已播目标续解后续）；磁盘不足时降级：播放不受影响，沿用旧窗口不滑动 |
| 批内切换 | 同一路径（窗口平移：已落地头部出窗、尾部补新），覆盖层宽限期（started）不清零 |
| 批外跳转 | 同一路径（窗口整体重算），started 重置 |

整窗已落地（全缓存）时不重定向——空 targets 会让解码线程判空退出——仅播放，job 字段仍切换
（Done 兜底补播以 entry_idx 为准）；focus 保持不动（锚点不因全缓存回退）。
注意 pending 表重定向后必须重建——旧批未落项已不在 targets 里永不落地，残留项会让批内
切换误判。cancel 标志只剩切包时置位（终止解码线程，会话随旧包丢弃）。

**收益**：随机跳看不再反复销毁/重建解码线程与重付 folder 定位成本；focus 抢道保证「正在
看的」优先落地；缓存命中与后台预取并行不悖；窗口随 focus 滑动，连续观看无批尾等待断崖。
VarTidy 仍走 extract_batch——一次性整批快照正适合 CLI 场景，不引入会话复杂度。

**测试**：解码线程侧 4 个单测（生命周期/复用抢道、回调链式追加目标、focus 漂移中断自愈、
gen 救回零落地轮）+ UI 层滑动窗口冒烟测试（模拟用户批内来回跳动，覆盖 keep/重定向/磁盘
降级/空闲退出全分支，终态逐字节比对）——断言明细与跳转链设计见 **DEVLOG §5**。

### 11.5 点击状态机：start_extract 分支链与日志对照（debug 手册）

> 完整手册已迁至 **DEVLOG §4**：两个正交变量（playing/focus）、五分支判定链
> （①去重早退→②keep→③整窗已落地→④磁盘门→⑤重定向）、日志对照表（grep 关键词
> 对号入座）、变量速查、典型时序回放、常见症状速查。分支判定机制（keep 语义、
> 滑动窗口重定向）的设计见 11.4。

---

## 12. VarTidy：var 资源包增量整理（CLI 子命令）

> 形态：`archview tidy` 命令行子命令，与播放器 GUI 共享同一个 exe 和 VFS 引擎，互不干扰
> （无参数启动 = GUI 播放器，行为完全不变）。var 文件本身不可播放不可预览，
> 整理功能与播放器 UI 无交集，因此走 CLI 而非塞进 GUI。
>
> 定位：替代手动五步流程——「解压整个包 → class_var.py 归类 → deal_invaild_files.py 搬运无效文件
> → rm_empty_dir.py 清理 → 手动并入仓库再人工去重」，实现零临时全量落盘的原地整理
> （50G 包手动流程需额外 ~100G 磁盘与两次全量搬运；新流程磁盘增量 ≈ 仓库确实缺少的 var 体积）。

### 12.1 核心原则（不可违反的约束）

1. **var_repo 对整理器只读 + 增量**：仓库是用户多年整理的资产。已有 var 文件与已有文件夹
   一律不动——不修改、不移动、不删除、不复写。整理器仅允许两个写动作：
   创建尚不存在的新作者子文件夹、向其中写入仓库没有的新 var。
2. **同名冲突仓库版胜出**：待合入 var 与仓库已有 var 同名时一律跳过、记入报告，
   代码中不存在任何"覆盖/替换"路径。
3. **无效文件不落盘**：包内非 var / 命名非法条目只进报告，不解压不落盘。
   对比手动流程"全部落盘 → 再搬进错误目录"：仓库零垃圾，也无需事后清理空目录。
4. **零临时全量落盘**：不整包解压，按 entry 直写仓库目标路径。

### 12.2 流水线

```
输入（单个包 / 目录递归扫描 / 多个包）
  → 逐包打开（复用 VFS 三格式实现、加密密码候选链路、分卷 RAR）
  → list() 平铺全部 entry（多级子目录天然覆盖）
  → 分类（class_var.py 规则移植为 classify.rs）：
      ├─ 合法：作者.内容.版本.var 四段校验（版本号纯数字、作者/内容非空）
      └─ 无效：非var / 格式错误 / 版本号错误 / 作者缺失 / 内容缺失 → 仅记报告
  → 包内去重（见 12.3）
  → 仓库去重：仓库/作者/文件名 已存在 → 跳过（仓库版胜出）
  → 磁盘余量预检：累计新增量 + 64MB 余量，不足直接报错（不做自动回退）
  → 按压缩包内部顺序批量抽取（复用 extract_batch / prefetch / 取消机制）
  → 直写 仓库/作者名/文件名（.part 临时名 + 原子 rename）
  → 汇总报告：新增 / 仓库已有 / 包内重复 / 同名不同内容警告 / 无效 / 失败
```

**解压顺序与落盘位置解耦**：抽取按包内顺序进行（固态包 folder 定位成本最低，
prefetch 顺带解出同 folder 后续目标摊薄成本），落盘按 `作者名/文件名` 定位。

**命名兼容**：作者名 = 按点切分第一段（strip 首尾空格），段内字符原样保留——
中文、空格、`&[]#%` 等 Windows 合法字符不影响分类与落盘（与 class_var.py 行为逐条对齐，
回归测试钉死；含中文/空格的压缩包文件名亦全链路支持）。
已知微小差异：Python `isdigit()` 接受全角数字而 Rust 用 `is_ascii_digit()` 更严格，
全角数字版本号会被判"版本号错误"——VaM 生态中不存在此形态，严格校验更安全。

### 12.3 两级去重

| 层级 | 场景 | 判据 | 处理 |
|---|---|---|---|
| 包内去重 | 同一包的不同子目录出现同名 var | 文件名 + CRC32（entry 元数据自带，零解压成本） | 同名同 CRC → 保留首个，其余记「包内重复」；同名不同 CRC → 跳过后续并记警告，交人工确认，绝不自动挑选（可能挑错版本） |
| 仓库去重 | 仓库/作者/文件名 已存在 | 文件名存在 | 跳过，保留仓库版（核心原则 2） |

### 12.4 写入安全

- `.part` 临时文件（文件名含 pid，防多实例踩踏）+ 原子 rename；
  rename 前二次检查目标存在性，已存在则丢弃 `.part` 保留仓库文件（防并发写入窗口）。
- 超长路径（>260 字符）走 `\\?\` 前缀。
- 逐包隔离：单个包打开/解压失败不中断批次，记入报告，最终汇总。
- 只读属性/权限导致的写失败：报错并记录，不静默跳过。

### 12.5 CLI 用法与「发送到」快捷方式

```
archview.exe tidy <包或目录> [更多包...] [选项]

  -o, --repo <dir>    目标仓库目录（缺省读 config.toml 的 vartidy.repo_dir）
  -n, --dry-run       预演：仅做分类与两级去重检查（纯元数据操作，不解压），
                      输出完整报告（新增 N / 仓库已有 M / 包内重复 K / 无效 J + 明细）
  -p, --password <pw> 加密包密码（可多次指定，按序尝试；config 顶层 passwords 兜底）
  -q, --quiet         静默：无控制台时结束不弹结果窗（脚本/计划任务防阻塞）
  --report <path>     报告输出目录（默认 stdout 摘要 + 报告文件，见 vartidy.report_dir）
```

`repo_dir` 收进 config.toml 后，资源管理器「发送到」菜单即可承载整理入口——零注册表写入、零管理员权限、
卸载无残留（SendTo 本就是 `%APPDATA%\Microsoft\Windows\SendTo` 文件夹的快捷方式枚举）。`sendto_setup.bat`
一键创建两个菜单项：放在 archview.exe 同目录（package.bat 打包后即如此）双击即可，exe 路径自动检测
（bat 顶部 `EXE_PATH` 可覆盖；同名覆盖重建，换 exe 位置重跑即可）；bat 内嵌 PowerShell `WScript.Shell`
COM 调用创建 .lnk（bat 无原生建快捷方式能力）。手动创建等价：`shell:sendto` 里新建快捷方式，目标填
`"<archview.exe 全路径>"`（播放器）或 `"<archview.exe 全路径>" tidy`（整理器）——不带 `"%1"`（注册表
shell 命令语法，快捷方式里不替换；选中文件由 Windows 自动追加为参数，多选天然支持）。工作目录设为
exe 所在目录，保证 config.toml 相对路径解析稳定。

- **Archview player**：GUI 模式启动时解析命令行参数（main.rs `initial_archive`）——压缩包文件
  直接打开；目录即媒体集合（递归收集图片/视频，跳过压缩包，见 4.2）。混合多选（目录+压缩包
  同时选中）按参数顺序取第一个有效项，其余忽略。首帧经 `open_path` 触发，与拖放同链路
  （目录走 DirVFS、压缩包走密码探测/弹窗/播放全复用）；非路径参数忽略，无参数启动行为不变。
  该入口同样覆盖拖到 exe 上、文件关联等一切「带路径启动」场景
- **Archview tidy**：对任意压缩包右键（可多选）→ 发送 → 即按配置的仓库执行整理。多包/目录批量场景用命令行或 bat

**加密包密码（CLI/SendTo 无交互输入的解法）**：尝试序列 = `[无密码] → CLI -p（按指定顺序）→ config 顶层 passwords`（与播放器公用同一列表）。
密码在**打开阶段即探测并锁定**（`vfs::open_with_password_probe`），不等到解压才暴露错误：
加密头包（7z/RAR）open 即验；内容加密包打开恒成功，故对包内**最小的加密条目**做密码校验——
ZIP 读 1 字节触发密码校验值、RAR5 头带 PswCheck（RAR4 无校验值，解压最小条目按 CRC 判定）、
7z 无校验值（解压最小加密条目到内存 sink + CRC 判定；条目加密与否由所在 folder 是否含
AES coder 检测）。探测命中后带锁定密码执行，执行期不再换候选重开；候选耗尽整包记失败、零痕迹回收。
选列表而非交互输入的原因：SendTo 无 stdin；N 个包批量逐个提示体验差；包密码来源固定（几个站点各一个），
小列表即真实心智模型。密码仅用于解压，不写入日志与报告。

**冒烟测试**：`test_tidy\setup.bat`（目录已 gitignore）一键全链路——构建 → `examples\make_smoke_fixtures`
生成测试源（明文/AES zip/明文/内容加密 7z + 既有仓库）→ dry-run → 实际整理（含加密包打开期密码锁定）→ 幂等重跑。

### 12.6 配置（config.toml 新增节，代码零硬编码）

```toml
# 加密包密码候选（播放器与 VarTidy 公用，按序尝试；打开阶段自动探测）
passwords = ["site-a 的密码", "site-b 的密码"]

[vartidy]
repo_dir = "D:/var_repo"   # var 仓库根目录（必须已存在；整理器只增量合入，绝不改动已有内容）
report_dir = "auto"        # 报告目录；"auto" = 首个输入包所在目录（不写入仓库）
```

### 12.7 复用清单（并入本仓库而非独立项目的理由）

| 复用能力 | 来源 | 对 var 整理的意义 |
|---|---|---|
| ZIP/7Z/RAR 读取、加密、分卷、密码候选 | `vfs/` 全部 | 下载包格式全覆盖，含 GBK 密码 |
| 7z 固态包 folder 直达（BlockDecoder） | 11.1 节优化 | 50G 级固态包定位目标秒级，否则分钟级全包解压——原地整理的前提 |
| extract_batch + prefetch + 取消 | 播放器慢路径同款 | 批量抽取摊薄固态定位成本 |
| .part + 原子 rename、余量预检、pid 临时名 | `cache.rs` 机制 | 仓库写入安全（12.4） |
| 后台任务 + 代际号模型 | OpenJob/ExtractJob | 多包排队处理的任务框架 |

### 12.8 实施状态与后续阶段

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1 | `src/vartidy/classify.rs` 移植 class_var.py 全部规则 | ✅ 完成（单测覆盖 5 类非法 + 合法 + 特殊字符命名） |
| 2 | 流水线：两级去重（CRC 判重）、余量预检、批量抽取直写、报告、dry-run | ✅ 完成（含仓库只读三道闸、密码耗尽零痕迹回收） |
| 3 | CLI 分发入口 + config `vartidy` 节 + 「发送到」（`sendto_setup.bat` 一键创建）+ `-q` 防脚本阻塞 | ✅ 完成（34/34 测试 + setup.bat 6 包冒烟全链路） |
| 3.5 | 密码打开期锁定：`open_with_password_probe`（三格式 verify_password，7z folder 级 AES 检测）+ passwords 提为播放器/CLI 公用顶层配置 | ✅ 完成（CLI 删执行期重开循环；播放器 OpenJob 探测链，弹框降级为兜底） |
| 4（可选） | 失败续跑（记录已完成包清单，重跑跳过） | 按需 |
| 5（可选） | hash 级去重（防"同名不同内容"漏判）、多包并发 | 按需 |

---

## 13. 渲染架构迁移：wid 原生子窗口 → mpv render API（单平面）

### 13.1 迁移动机：wid 双平面的先天缺陷

MVP 采用 mpv 官方推荐的 Windows 嵌入方式：`SetParent` 把 mpv 原生子窗口塞进 egui
窗口，视频画在子窗口、UI 画在父窗口——**两个独立平面，mpv 子窗口永远盖在 egui 之上**。
由此产生一整串绕路代码（全部随迁移删除）：

| 绕路 | 位置 | 问题 |
|---|---|---|
| 子窗口创建/同步/销毁 | ui.rs `create_child` / `sync_child_rect` / `sync_child_visibility` | 每帧 SetWindowPos 换算物理像素；显示状态手动同步 |
| 覆盖层无法盖住视频 | 解压进度覆盖层、音量提示等 | 必须反 hid 子窗口才能露出 egui 内容（11.3） |
| 弹窗/菜单遮挡 | 密码框、确认框 | 同上，全要绕 |
| 画面缩放质量 | mpv 独立平面自行缩放 | 窗口缩放时锯齿/模糊不受控，无法与 egui 同步重采样 |
| 多显示器 DPI | 子窗口像素坐标换算 | 整数/分数缩放下错位闪烁 |

render API 下 mpv 只做解码 + 渲染到宿主提供的 FBO，**整个窗口只有 egui 一个 GL 平面**，
以上问题全部不存在。

### 13.2 新渲染链路

```
mpv (vo=libmpv, hwdec=auto-copy)
  │ mpv_render_context_render → 宿主 glow FBO（离屏纹理）
  ▼
egui PaintCallback（video_area rect 处）
  │ 1. bind READ/DRAW_FRAMEBUFFER = 宿主 FBO，viewport 其尺寸
  │ 2. 需要新帧时调 mpv_render_context_render（flip-y=1 适配 GL 底朝上）
  │ 3. gl.blit_framebuffer 到默认帧缓冲的目标 rect（y 轴翻转换算，LINEAR 过滤）
  │ 4. 恢复 egui_glow 的 framebuffer 绑定（状态封闭，不污染后续绘制）
  ▼
egui 绘制其余 UI（覆盖层/进度条天然与画面同平面叠放）
```

关键实现点：

- **glow 后端**：Cargo.toml `eframe = { default-features = false, features = ["glow", ...] }`
  + `NativeOptions.renderer = Renderer::Glow`。0.34 起 eframe 默认 wgpu，glow 才有
  `painter.gl()`（glow::Context）可喂给 mpv 的 get_proc_address。
- **FFI 符号**（mpv_ffi.rs 追加）：`mpv_render_context_create/free/render/set_update_callback/update`
  + 参数结构 `mpv_render_param{type,data}`（`{INVALID,NULL}` 结尾）、
  `mpv_opengl_init_params{get_proc_address,ctx}`、`mpv_opengl_fbo{fbo,w,h,internal_format}`
  （宽高在结构体内）。精确枚举值：API_TYPE=1 / OPENGL_INIT_PARAMS=2 / OPENGL_FBO=3 /
  FLIP_Y=4 / **BLOCK_FOR_TARGET_TIME=12**。
- **`vo=libmpv` 必须在 create_render_context 之前设置**；`video-timing-offset=0` +
  render 传 `BLOCK_FOR_TARGET_TIME=0`（render.h 官方建议组合）：render 永不阻塞。
  hwdec=auto 在 render 模式下 mpv 自动落 copy 模式，配置不变。
- **帧呈现节奏（vsync 主钟）**：render() 默认（BLOCK_FOR_TARGET_TIME=1）阻塞调用
  线程到帧目标显示时刻——UI 线程被卡在 render 内部（mpv 看门狗报 "not being
  called or stuck"，实测首帧阻塞 ~550ms）；叠加"update 回调唤醒 → 事件循环调度"
  链路上 1~15ms 的毫秒级抖动，30fps 帧的呈现时刻在 vsync 边界随机跨越（帧长
  2/3 个 vsync 交替）→ 可感知卡顿。修复：视频播放中（ui/mod.rs `video_live`：
  playing 且非图片且非暂停）逐 vsync 连续 `request_repaint()`，eframe 的 vsync
  swap 成为节拍主钟；render 每次"当前应显示"的帧（BLOCK=0），换帧时刻锁定
  vsync 边界，残余 judder 仅剩 fps 与刷新率的时钟漂移（ppm 级）。重复渲染同帧
  无害（mpv 重画上一帧）；A/V 同步误差 ≤1 个 vsync（低于感知阈值）。暂停/图片
  模式回落 100ms 慢节拍省功耗。
- **播放状态全订阅缓存（observe_property，修复叙事见 DEVLOG §3）**：pause /
  eof-reached / pause-for-cache（FLAG → AtomicBool）+ time-pos /
  duration（DOUBLE → AtomicU64 存 f64 位模式）共五属性经 `observe_property`
  订阅推送，常驻事件泵 `MPV_EVENT_PROPERTY_CHANGE` 按格式分发（FLAG 读
  `*const c_int`、DOUBLE 读 `*const f64`）。**UI 帧路径零 `get_property`**——
  get_property 是同步 FFI（拿 mpv core 锁），core 忙窗口（seek/打开/hwdec 试探）
  排队可阻塞上百 ms（实测 seek 后 194ms 整帧冻结）；订阅缓存读取是纯原子操作
  纳秒级。订阅洪流上限 ≈ 帧率（time-pos ~60 条/s），泵消费百纳秒级/条。
  mute 不走订阅：静音状态唯一真相在 `cfg.player.muted`（持久化，未播放/
  未开文件也可预设），UI 直读写 cfg，播放器创建时与 volume 一起应用、
  在场变更经 `set_muted` 走 `mpv_set_property_async` 异步下发——入队即
  返回不拿 core 锁，打开忙窗口内点静音/拖音量 UI 线程零阻塞（同步
  set_property 与 get_property 同锁，忙窗口内会阻塞 UI）；写失败只有
  回复事件（泵不消费），cfg 是真相、下次创建对齐。无 mpv 回读需求，
  还省一条订阅。
- **装载窗口期门控（player/mod.rs `opening()`）**：`load_mark.is_some() &&
  !painted_since_load`（2s 兜底）——订阅化后其防冻结职责已消失（帧路径不再进
  mpv），保留的是正确性职责：装载期新文件的属性事件尚未到达，原子缓存里是
  上一文件的旧值，窗口期内短路返回占位值。
- **帧节奏探针（常驻诊断，panels.rs `Cadence` thread_local）**：每 5s 汇总
  paints/avg/max/长帧归因（render 侧 = 本帧 render>10ms，撞 core 锁或首帧 GLSL
  编译；ui 侧 = egui 布局/驱动 swap/OS 调度）/mpv 换帧数/render 最重耗时。
  `debug` 级默认静默，诊断时 RUST_LOG 环境变量或 config.toml
  `log_level = "info,archview::ui::panels=debug"`（优先级 RUST_LOG > config > info，
  见 main.rs 启动序）。
  基线（release，60Hz/30fps）：avg=16.7ms 恒定、render最重 <1ms、
  ui 侧长帧 ~2 个/5s（OS 底噪，丢 1 个 vsync tick 不可感知）、
  首窗 render 侧 1~2 个 70~90ms（首帧 GLSL 编译，进程一次，不预热）。
  探针家族（帧探针/seek 探针）与日志级别策略见 DEVLOG §3.4。
- **FFI 闭包陷阱**：`Arc<dyn Fn>` 是双字 fat pointer，user_data 只收单指针——
  先 `Box::into_raw(Box::new(arc))` 变 thin pointer，释放时按原型回收。
- **销毁顺序**（全部在 GL current 线程，eframe `on_exit` 回调内，早于 App::drop）：
  `set_update_callback(None)` → `render_context_free` → 回收两个 user_data Box →
  删 glow FBO/纹理；`Drop` 只剩 mpv terminate_destroy。
- **生命周期**：render ctx 必须先于 mpv handle 释放，否则 terminate 时 mpv 在无 GL
  环境下清理渲染资源 → 崩溃。

### 13.3 实施步骤（✅ 已全部完成）

1. ✅ eframe 切 glow 后端（Cargo.toml + main.rs `Renderer::Glow`）；
2. ✅ mpv_ffi.rs 扩展 render API 符号（5 函数指针 + 3 repr(C) 结构 + 常量）；
3. ✅ player/mod.rs 新增 render context 管理（创建/更新回调/FBO 维护/render 调用）；
4. ✅ ui.rs video_area 集成 PaintCallback blit；extract_wait 改为"跳过该帧视频绘制"；
5. ✅ 验证后删除 wid 全套绕路代码：child_hwnd/child_hidden/overlay_open/native_hwnd/
   create_child/sync_child_rect/sync_child_visibility/DestroyWindow/frame 参数链。
   wid 路径不留开关、不做回退——双平面问题根除后无理由保留。

> 迁移过程中的三颗实际踩过的雷（FFI 盲区崩溃 / blit READ 绑定套娃虚影 /
> smooth_scroll_delta 惯性尾巴）见 DEVLOG §2 踩坑记录（共 9 坑）。

### 13.4 图片专属纹理浏览器（第 1-4 步 ✅ 已落地；5 按需）

render API 迁移已完成（13.3），GL 上下文与 PaintCallback 机制就绪，本节从"预留
扩展"（6.4）升级为实施计划。能力收益对照见 6.4 表格；这里是落地方案与实况。

**动机**：图片曾与视频同走 mpv loadfile，首帧带 50~200ms 的 demuxer/GL 管线
初始化（翻页体感顿挫）；且 mpv 不支持的格式（PSD/HEIC/AVIF）永久缺位。图片
不走 mpv、直接解码进 egui 纹理，可与 mpv 路径零成本并存。

**数据流**（VFS 层零改动——两条路径只是同一 VirtualFile 流的不同消费者）：

```
App::play → wants_texture(cfg, name)          # image_texture 开 && 图片扩展名
  → img_source = (key, source, name) 记录      # extract.rs（full 升级回查数据源）
  → ImageService::request(key, source, max_dim, name)
      spawn "imgdec" 线程 → decode_image(_, full=false, ppp, &name)
      → mpsc ImgMsg::Ready → poll() 上传纹理进 LRU → ImgEvent::Ready → UI 换画
深放大时（panels.rs 触发判据见下）
  → ImageService::request_full(key, source, name)
      spawn "imgfull" 线程 → decode_image(_, full=true, ppp×2, &name)
      → mpsc ImgMsg::FullReady → poll() 上传 full 槽 → 下帧无缝换画
```

**路由**（✅）：play() 顶部统一密码检查后按扩展名分流（`imgview::wants_texture`：
静态图 + GIF / 动画 webp 动图 + `ui.image_texture`
开关；**动图也走纹理直显，mpv 收缩为纯视频播放器**）——流式 zip/目录图走
`play_image()`（不解压、不 ensure_player），7z/RAR 图照常解压会话，FileDone/
缓存命中经 `load_extracted()` 分流纹理解码。webp 动/静在 `decode_any` 内经
`WebPDecoder::has_animation()`（VP8X ANIM 位）分流：动画走 gif 同款 `into_frames`
逐帧（帧迭代产画布尺寸合成帧 + 毫秒延迟，与 gif 完全同构），静态走单帧
decode（不可盲目逐帧——静态 `num_frames()==0` 空集误报，DEVLOG §2 坑 4）；
`new()` 时头已解析，检查零额外成本。纹理解码失败（格式不支持/数据
损坏）经 `ImgEvent::Failed` **自动落回 mpv**（流式 load 流 URL / 落盘 load_path），
两条路径互为兜底；config `ui.image_texture` 开关一键整体回退。

**回退链路**（✅）：纹理解码失败回退 mpv 时，`img_fallback: HashSet<ImgKey>`
记入 + `tex_image_key()` 剔除 + `prefetch_images()` 跳过——否则 mpv 已解出首帧
却被图片占位永久遮挡。踩坑过程见 DEVLOG §2 坑 4。

**交互**（✅，参考 Honeyview 等主流看图软件）：
- 绘制：`Mesh::with_texture + add_rect_with_uv` + `painter_at(媒体区)` 裁剪；
  mpv PaintCallback 以 `tex_image_key().is_none()` 互斥；绘制矩形做**物理像素
  对齐**（DEVLOG §2 坑 8——自绘 mesh 不 snap 时 100% 也会整幅混糊）；
- 缩放状态 `ImgView { zoom: Option<f32>, pan }`：None=适应窗口；zoom 范围
  `MIN_ZOOM` 0.05 ~ `MAX_ZOOM` 256；纯几何函数
  `fit_scale`/`zoom_pan`/`clamp_pan` 独立单测（clamp 语义：大图不露边、小图归零居中）；
- 拖拽平移（图像中心 clamp 在视口内，拖不丢）；滚轮**模式感知**：fit 态 = 翻页（`ui.wheel_switch_secs` 节流）；缩放态 =
  锚光标 ±1.2×（上滚放大），**缩小触底自动落 fit**（判据
  `(zoom×factor).max(MIN_ZOOM) <= fit`；fit_scale 不封顶，小图 fit>1 同样
  成立），落 fit 后继续滚即翻页，无需双击退出；**Ctrl+滚轮**任意态（含 fit）
  直接起缩放。⚠️ 触底判据仅存在于滚轮缩小路径——双击/键盘 100% 无条件写
  zoom，不会被拉回 fit；
- 双击 = 适应窗口 ↔ 100%（锚光标），**100% 写 `z100 = 1.0/ppp`**（zoom=1.0
  ≠ 100%，高 DPI 屏写 1.0 会过扫 + 误触 full 升级，见下「zoom 语义」）；
  **全屏不再绑双击**（Enter/Esc 照旧）；
- 键盘：←/→ 上一张/下一张；`+`/`−` 1.25× 步进（锚视口中心）；`0`=100%
  （`img_zoom_to(Some(1.0))` 内部 `z/ppp` 换算）；`Home`=适应窗口；
- 模式徽标（媒体区右下角 `paint_badge`，半透明黑底圆角）：缩放态常驻
  `NNN%`（`zoom×ppp×100` 取整，100% = 纹素:物理 1:1 基准）；任何路径落回
  fit（滚轮触底/双击/Home）闪现"适应窗口"约 1s（`img_fit_flash` +
  `request_repaint_after` 驱动消失），fit 阅读态无徽标；
- 滚轮在 `frame()` **之前**处理（DEVLOG §2 坑 6：占位期间也要能翻页逃生），缩放
  推迟到交互段（需基础纹理落地后的 zoom/fit 值）。

**zoom 语义（定稿，勿回退）**：显示物理尺寸 = 基础纹理 × zoom × ppp（ppp =
屏幕像素密度，150% 屏 = 1.5）。由此：

| 概念 | 值 | 含义 |
|---|---|---|
| fit | `fit_scale(viewport, tex)` | 适应窗口（zoom=None 实时值），**不封顶**——小图 fit>1（fit 态即放大铺满） |
| **100%** | **`z100 = 1.0 / ppp`**（150% 屏 ≈ 0.667） | 1 纹素 = 1 物理像素，零重采样最锐点 |
| full 触发/换画判据 | `zoom × ppp > image_full_threshold`（默认 2.0，config 可调） | 100%~195% 档基础纹理 GPU 直放大即可（full ×2 纹理在此区间反被 0.78–0.98× 缩小采样，其 ~2px 边缘坡与 base 双线性等宽，195% 实拍 A/B 无差）；越过 244% 档（×1.25⁴=2.441）才换 full，相对 full 采样比 ≥1.22× 纯放大有真增益——死区取 2.0 而非恰等档位的 2.441，防 `>` 判据浮点踩空与临界抖动；调小（如 1.4）更早升级对比，调大（如 3.0）基本关闭 |

基础纹理本就按屏幕密度（×ppp）预放大，"1 纹素 = 1 物理像素" 即原生 1:1 点，
不需要知道原图尺寸、解码链零透传。曾评估"往 Cached 存原图尺寸、100% 定义为
原图 1:1（zoom=1）"的方案，已推翻：需 ImgMsg/闭包全链透传 native 尺寸，且
zoom=1 时 `zoom×ppp = ppp > image_full_threshold` 会误触 full 升级，判据不自洽。⚠️ 高 DPI 屏
写 `Some(1.0)` 冒充 100% 是 bug：物理过扫 ×ppp 且误触 full（LOG2 事故根因）；
换算在双击与 `img_zoom_to` 两处，漏一处即翻车。

**两档纹理体系（resample 策略）**：`decode_image(source, max_dim, full,
factor, name)` 同一解码函数两档复用，factor 由 ImageService 按屏幕 DPI 内部
取值、调用方不传：

| | 基础档（full=false） | 高清档（full=true） |
|---|---|---|
| 去处 | LRU 缓存（多张，±1 预取） | full 单槽（一张，切条目即弃） |
| 倍率 | ×ppp（低密屏钳 1.0） | ×ppp×2（150% 屏 = ×3） |
| 类型 | **阈值型**：native×ppp ≤ `image_max_dim`(4096) 才 Lanczos 预放大；**超限原尺寸直进**（超大图密度已足，免降采样损失与秒级重采样） | **clamp 型**：无条件 ×factor，封顶 `FULL_MAX_DIM`(8192) |
| 病态兜底 | 最长边 > 8192 一律降采样（两档共用） | 同左 |
| 动图 | 不预放大（多帧 ×factor² 爆显存）；超限降采样、8192 硬上限同样约束 | **不升级**（`is_static` 前置过滤） |

**级联放大禁令（不可违反）**：full 档必须**从原图重新解码** Lanczos ×(ppp×2)，
绝不在基础纹理（已 ×ppp）上级联再放大——两级重采样抹锐边。升级资格判据
`native ≤ image_max_dim` 与基础档阈值恒等价（预放大的图 base=原图×ppp ≤ 上限；
直进的图 base=原图 > 上限自动出局），无需额外状态。

**高清升级（深放大按需解码，✅）**：GPU 固定管线放大只有双线性，深放大细节
受限。触发判据（panels.rs 每帧检查，幂等由 ImageService 保证）：

```rust
native_max <= image_max_dim      // 升级资格（见级联放大禁令）
&& zoom * ppp > image_full_threshold  // 越过 244% 滚轮档才升级（默认 2.0 config 可调；更浅档 base 直放大肉眼等价，不为此白付二次解码）
&& img.is_static(key)            // 动图不升级
&& !has_full && !is_full_pending && !is_full_failed   // 幂等/失败不重试
```

- 换画判据（绘制时）：`screen_px = native×zoom×ppp > native.max_elem()×image_full_threshold`
  才用 full 纹理，否则基础纹理（100%~195% 档 base GPU 直放大与 full 肉眼
  等价——195% 实拍 A/B 无差；判据与触发同配置，两处必须一致否则触发/换画
  错位）。full 上传用 TEX_OPTS_FULL **无 mipmap**：默认阈值 2.0 下 full
  工作区采样比 >1.0（244% 档起 ≥1.22×）纯放大，LOD 恒 ≤0 用不到 mip 链；
  若调低阈值让 full 落入缩小区（如 195% 档 0.98×），开 mipmap 三线性会
  混入半分辨率 mip1 把 2× Lanczos 增益打回 base 水平，无 mipmap 直采仅
  轻微欠采样，静止图无感。base 档保留 mipmap（fit 态深缩小防摩尔纹）；
- ×ppp×2 的密度保证放大到 ×4 原图尺寸前残余重采样 ≤×1.25（近 1:1）；再深
  交给 GPU 双线性（继续升级收益 <×1.4，不值二次解码秒级 + 180MB 级显存）；
- 锐化（`image_full_sharpen` 配置，默认开，仅高清档）：Lanczos 放大后叠加
  unsharp mask——`out = px + 0.6×(px − blur_σ1.0(px))`，仅 RGB、alpha 直通。
  插值本质会把边缘坡度摊平（1px 边缘摊成 factor px 缓坡），锐化把坡度
  重新变陡，深放大观感明显改善（信息量不变，纯观感补偿；强对比边缘可能
  有轻微光晕，关配置可 A/B）。后台线程 O(n) 执行不卡 UI，解码日志带
  "锐化"标记即本帧已叠加；
- 无缝换画：full 与基础纹理同宽高比，屏幕矩形/UV 一律按**基础纹理**几何算
  （UV 0..1 对两种分辨率通用）→ 只换 `tex.id()`
  （`full_tex().unwrap_or(基础纹理)`），解码期间基础纹理照常显示，无消失/
  闪烁/几何跳变；
- 单槽 `full: Option<(key, tex)>` 切条目即弃（×2 显存 ×4，不进 LRU）；三态
  防重入（就绪/在途/已放弃）+ 专属看门狗（10s 迟到判死）——zoom 在触发
  边界来回变化不重复解码；`img_source` 由 play_image/load_extracted/heal
  记录、切视频即清（full 升级要回查数据源）。

**切图防闪烁（HoldImg 快照，✅）**：非 solid 大图解压+解码可达数秒，切换期间
`frame()=None` 时**优先原样重画上一张快照**（`hold_img { tex, rect, since }`，
每次成功绘制后刷新；等待 <2s 期间 50ms 高频轮询）而非直接跳占位；无快照
（首图）或超 2s（疑似无限解压，用户需明确感知）退回"正在解码图片… 已等待
Ns"占位（200ms 轮询）；切视频三处清快照防旧图残留。

**内存控制**（✅，`src/imgview.rs` ImageService）：
- 后台线程解码（zip/dir 走 VFS 流读、7z/RAR 读落盘缓存），mpsc 回 UI 线程
  `poll()` 上传纹理；
- 两档密度适配（策略见上表，`resample` 单测钉死三种形态：基础档按密度预
  放大 / 高清档 clamp 放大 / 病态图 8192 兜底）——翻页即原生密度直出、
  100% 零重采样；mipmap 仅 base 档（TEX_OPTS，补 fit 态深缩小质量），full 档
  TEX_OPTS_FULL 无 mipmap（见换画判据：mip 混合会抹掉 2× Lanczos 增益）；
- ±1 张预取的 LRU 纹理缓存（容量 = `image_cache_count`；逐出跳过刚推入与
  正在显示（`shown`）两个 key，都不堪逐时允许临时超容、下次推入再收缩）：
  连续翻页即显，看过的图翻回秒开；
- 调试日志（debug 级，按链路时序）：`发起解码`（request）/ `发起高清解码`
  （request_full——出现即深放大触发）/ `解码 {fmt} native_size=WxH
  {lru|full}_size=WxH[ 锐化]: Xms`（lru 尺寸 == native 即原样直进未重采样；
  "锐化"标记 = 本帧叠加了 unsharp mask，A/B 对比看这个；启动 info 级
  `图片高清升级: 阈值×N 锐化 开|关` 回显两配置生效值）/
  `纹理上传[..] lru|full: Xms` / `换画[(id, idx)] badge=N% base=WxH → full
  WxH|→ base`（同 key 档位翻转时才一条，切图新 key 静默重置）/ `原图解码超时
  （>10s）放弃`（full 看门狗）；
- 防死局三件套：pending 看门狗 10s 剔除可重试、`catch_unwind`
  捕获 worker panic 转 Failed 走回退（否则线程静默死亡，pending 永久泄漏 →
  幂等检查拦截 → 永久占位死局）、`heal_current_image()` 每帧自愈（当前图
  不在缓存且无在途解码就补发请求）——杜绝任何形态的"永久占位"；
- TextureHandle 随 LRU 淘汰自动释放，切包/退出随 App 释放。

**实施顺序**（每步独立可交付）：
1. ✅ 静态图纹理路径 + 路由 + 失败回退（核心收益：翻页零延迟）；
2. ✅ 滚轮缩放（锚定光标）/ 拖拽平移 / 双击 fit↔100% + 快捷键（+/−/↑↓/0/Home，
   ↑/↓ 图片=缩放、视频=音量，模式分治）；
3. ✅ ±1 预取 LRU；
4. ✅ GIF / 动画 webp 逐帧解码接管（image crate `AnimationDecoder::into_frames`
   全帧预解码 + 逐帧纹理；mpv 图片链路的 5s 黑屏/200ms 空洞问题随之整体消失；
   **预取对动图生效**——mpv 路线动图走 loadfile 无法进 LRU，这是决定性差距）：
   - **webp 经 has_animation 分流**：VP8X ANIM 位置位走 gif 同款逐帧，
     静态走单帧 decode——不可盲走逐帧：静态 webp 无 ANMF 块，image-webp
     `num_frames()==0`，`into_frames()` 解出空集 → 误报"动画无有效帧"
     回退 mpv（LOG2 实测踩坑，DEVLOG §2 坑 4）；回归测试
     `decode_静态webp归静态` / `decode_动画webp逐帧` 各锁一路径；
   - 静态归一：解出 1 帧自动归 `Decoded::Static`（零时钟开销），判定依据是
     解码结果而非扩展名/头部；
   - 播放：`Cached { frames, delays, cur, next_at }` 状态机，UI 层取帧时推进
     时钟并 `request_repaint_after(距下次切帧)` 驱动，稳态功耗 = 帧率；
   - 帧延迟 < 20ms 钳 100ms（浏览器惯例，防 0 延迟坏帧疯闪）；帧数封顶
     512（截断优于回退 mpv）；动图不上采样（多帧 ×4 显存不值当）；
   - 附带：mpv 装载窗口 16ms 轮询（`has_painted_since_load`）——视频/回退路径
     切换时 UI 高频 repaint 解除 mpv VO 等待，首帧画出即停（消两段 ~200ms 空洞）。
5. PSD/HEIC/AVIF 扩展解码器（按需，接库走同纹理路径）。

### 13.5 踩坑记录（已迁至 DEVLOG §2）

九坑完整记录（现象→根因→修复→教训）见 **DEVLOG §2**：FFI 签名错 0xc000041D /
blit READ 绑定套娃虚影 / smooth_scroll_delta 惯性尾巴 / 静态 webp 空集+回退遮挡 /
LRU 挤出正在显示 / 占位分支早退跳输入 / debug 构建解码慢 / 物理像素对齐 /
IDE 旧缓冲覆盖磁盘编辑。本文各处单坑引用格式为 `DEVLOG §2 坑 N`。

## 14. 底部控制栏布局机制（egui 0.36）

> 完整踩坑叙事（五坑 + 字形级测量方法）见 **DEVLOG §7**；本节只留机制与不变量。

### 14.1 目标与结构

目标三条：播放键中心恒在窗口水平中线（零手估像素、零逐帧计算）；行底到窗口底
恒 12px；图↔视频模式切换按钮组零位移。

```
Panel::bottom("controls")              ← 锚定窗口底缘；frame inner bottom = 默认2 + 10
 ├─ seekbar 槽 18px（两模式同尺寸；图片模式隐形占位，保面板高度一致）
 └─ 行带 allocate_ui_with_layout([宽, 34], left_to_right(Min))   ← 高度必须固定
     ├─ 音量组（先画，仅视频）right_to_left(Center)：数字槽 26×34 → 滑条 84×34 → ♪ 按钮 36×34
     └─ Aligner::center().bounds(Bounds::max_rect()).show_horizontal   ← 五槽对称按钮组
         [隐形48][< 48][播放 48][> 48][模式键48 / 隐形48]
```

### 14.2 播放键居中：sizing-pass 两遍布局 + 五槽对称

- **Aligner**（egui_alignments crate，与 egui 同版本锁定）：egui 官方 sizing-pass
  机制——首帧按可用宽度隐形测量内容实际宽度并 `request_discard` 丢弃该帧，次帧
  起按实测宽度精确定位；内容宽度变化（字体/DPI/按钮尺寸）自动重测。无手估像素、
  无逐帧反馈计算。居中参照 `Bounds::max_rect()`（整行矩形），**忽略行内已占用
  空间**——因此音量组先画不影响按钮组落点（顺序反转是刻意为之）。
- **五槽等宽 48 对称**：播放键两侧各两槽 → 播放键恰为组的几何中心，Aligner
  居组＝播放键中心落在窗口中线（四槽组时播放键偏 -28px，见 DEVLOG §7 坑3）。
  视频↔图片模式第五槽一实（↻/≡ 模式键）一隐（等宽占位），组宽恒定，切换零位移。

### 14.3 不变量（破坏即回归，对应坑见 DEVLOG §7）

| # | 不变量 | 破坏后果 |
|---|---|---|
| 1 | 行带固定 34 高（`allocate_ui_with_layout`），不用 `ui.horizontal` | 自动高度面板的"当帧可用高度"来自上一帧面板高度，行高自流会与 Aligner 记忆尺寸形成反馈环：实测图片模式面板 55→47 压矮、行带挤到 18、按钮溢出绘制（坑4） |
| 2 | ♪ 所在 ui 的 layout 交叉轴必须 Center | egui 0.36 Button 内部文本对齐**默认继承所在 ui 的 `layout().vertical_align()`**（atom_layout.rs align2 回退）；Min → ♪ 文本在 34px 按钮内贴顶，字形上移 6px（坑5）。行带固定高后 Center 不会引发膨胀——膨胀真凶是行带不固定高，不是 Center |
| 3 | 行底间距只由面板 frame inner bottom 决定（当前 12） | bottom 面板锚定窗口底缘 + 行带是面板末元素 → 行底间距恒定、不随内容高度变化。不要用行尾 `add_space` 垫底：实测实推 7px 且位置随行内分配漂移（坑2） |
| 4 | 行内子项一律显式 34px 等高（音量组三个子项、按钮组全部按钮） | 高度不齐时交叉轴对齐差异可见；等高时 Min/Center 对矩形位置无影响，只通过不变量 2 传导给 Button 文本 |

另：egui 0.36 `Margin` 字段是 `i8` 整型（`frame.inner_margin.bottom += 10`），
写 `+= 4.0` 直接 E0277 编译错（坑1）。

### 14.4 验证方式

无头字形级测量：`Context::run_ui` 复刻行结构跑多帧，从 `FullOutput.shapes` 提取
♪/▶ 的 `TextShape`，用 `galley.rect`（文本框）与 `galley.mesh_bounds`（墨迹紧致
包围盒）对齐按钮 `Response.rect`——能测出肉眼级的 6px 回归。方法与数据见
DEVLOG §7.6。最近一次全量回归（12px 版）：行底距窗口底 12.0、播放键中心偏差 0.0、
♪ 文本框居中偏差 0.0、两模式面板高度帧间稳定。
