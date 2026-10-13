# 压缩包媒体预览播放器 — 架构设计

> 目标：打开一个压缩包（RAR / 7Z / ZIP），浏览其中分散在各子目录的视频与图片文件，
> 视频可直接播放（支持拖动进度条 seek），图片可预览（类 HoneyView 体验），无需完整解压。

---

## 1. 需求概述

| 需求 | 说明 |
|---|---|
| 压缩包格式 | ZIP、7Z、RAR（含 RAR4/RAR5、分卷） |
| 媒体类型 | 视频（播放）、图片（预览），文件分散在压缩包任意子目录 |
| 播放能力 | 内嵌播放、拖动进度条 seek、暂停/音量等常规控制 |
| 图片预览 | 缩放/平移/适应窗口、上一张/下一张、动图(GIF/WebP) |
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
| 视频播放 | 自研 `mpv_ffi` | libloading 运行时动态加载 libmpv-2.dll + 手写最小 FFI 绑定（约 15 符号，含 stream_cb）。不用 libmpv-rs：shinchiro 构建的 libmpv.dll.a 是 MinGW 导入库，MSVC 链接不了 |
| 图片渲染 | —— | 无独立解码库：图片（含 GIF/WebP 动图）与视频统一经 mpv `loadfile` 渲染，VFS/缓存/翻页链路完全复用 |
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
  ├─ ZIP ────────────── entry 独立压缩，seek 便宜 ──→ 流式按需读取（快路径）
  ├─ 7Z / RAR 非固态 ── 单文件独立可解 ──────────→ 流式按需读取（快路径）
  └─ 7Z / RAR 固态(solid) ── seek 需从固态块起点重解 ──→ 后台解压到缓存目录（慢路径）
                                                        解压至 N MB 即起播，边解边播
```

> 注意：RAR5 默认固态，7z 也常用固态 —— 慢路径是**必选实现**，不是优化项。
> 本地磁盘解压速度远超播放码率，起播延迟通常 1~3 秒。

---

## 3. 整体架构

```
┌─────────────────────────────────────────────────────┐
│  UI 层 (egui)                                       │
│  左侧：文件列表（视频/图片标签页）+ 搜索            │
│  右侧：媒体区 —— mpv 原生子窗口统一渲染             │
│         （视频与图片/动图同一条 loadfile 链路）      │
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
     │ Read + Seek       │
┌────▼───────────────────▼─────────────────────────────┐
│  VFS 层（核心抽象，播放与整理共用）                 │
│  trait ArchiveVFS { list(); open_entry() }           │
│  trait VirtualFile: Read + Seek { size() }           │
│  ├─ ZipVFS    （随机访问）                           │
│  ├─ SevenzVFS （BlockDecoder 固态 folder 直达）      │
│  └─ RarVFS    （探测固态；分卷合并处理）             │
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

---

## 5. Seek（拖动进度条）详细设计

### 5.1 结论

**完全支持。** 进度条拖动在 mpv 中即 `seek` 命令 → demuxer 将时间换算为字节偏移 →
触发 stream_cb 的 seek 回调 → `VirtualFile::seek()`。链路完整，差异仅在性能，
按下表分级处理。

### 5.2 各格式 seek 成本

| 格式/模式 | 前向 seek | 后向 seek | 说明 |
|---|---|---|---|
| ZIP (store) | O(1) | O(1) | 直接偏移定位 |
| ZIP (deflate) | 便宜（跳读解压输出） | **贵**（从 entry 头重新 inflate） | 需解码块缓存缓解 |
| 7Z 非固态 | 便宜 | **贵**（LZMA 从 entry 头重解） | 同上 |
| RAR 非固态 | 便宜 | **贵** | 同上 |
| 任意格式固态（慢路径） | 需解压推进到目标点 | 同左 | seek = 解压任务重定向 |

> 多 GB 视频在 deflate/LZMA 下的后向 seek 若无缓存，可能需数秒 —— 因此以下缓解措施为**必选组件**。

### 5.3 缓解措施（四层）

1. **mpv demuxer 缓存**：调大 `demuxer-max-bytes` / `cache-secs`（config.toml 可配）。
   小幅回跳（刚看过的地方）命中缓存，零成本 —— 覆盖最常见的拖拽场景。
2. **解码块 LRU 缓存（快路径）**：解压后的数据按块（默认 4MB/块，可配）进 LRU；
   后向 seek 落在缓存内则 O(1)。播放为顺序读，"拖回刚才看过的地方"命中率极高。
3. **解压任务重定向（慢路径/固态）**：seek 超出已解压区域时，解压管理器终止当前任务，
   从目标 offset 所在固态块重起解压（途中数据只解不写）。UI 状态栏显示"跳转中…"。
4. **读取阻塞即缓冲**：stream_cb 的 read/seek 回调在 mpv 缓存线程上同步执行，
   阻塞是安全的（与网络流一致），mpv 自动显示 buffering 动画。

### 5.4 交互细节

- UI 进度条：观察 mpv `time-pos` 属性驱动进度显示；拖动松手后发 `seek` 命令（拖动中不连续发）。
- 慢 seek 期间：UI 显示 mpv buffering 状态；连续拖动产生的新 seek 取消前一个解压重定向任务。
- 快路径后向 seek 且缓存未命中：在 read 回调中同步重解压，期间 mpv 自然缓冲。

---

## 6. 图片预览模块

### 6.1 定位与实现决策

与视频播放并列的 VFS 消费者。**图片不经独立解码库，统一走 mpv 渲染**
（早期设计稿的 image crate + egui 纹理方案已放弃）：mpv 原生支持 JPEG/PNG/GIF/WebP/BMP
等常见格式（含 GIF/WebP 动图逐帧播放），复用 loadfile 一条链路后，VFS 快慢路径、
缓存、翻页导航、全屏、解压进度覆盖层全部零成本复用，也避免了
"mpv 子窗口永远盖在 egui 之上"（见 11.3）与纹理渲染的窗口叠放冲突。

### 6.2 数据流

```
选中图片 → 与视频完全同路：ZipVFS 流式（zvf://）或固态包后台解压 → 缓存文件
        → mpv loadfile 统一渲染（静态图 / GIF/WebP 动图）
        → PPT 式自动翻页（ui.slideshow_secs 间隔，空格/播放按钮开关）
```

### 6.3 功能与交互

| 功能 | 说明 |
|---|---|
| 翻页 | PgUp/PgDn 切换上/下一个；滚轮仅在**视频区**悬停时切换（`ui.wheel_switch_secs` 节流）——文件列表区滚轮是正常滚动列表，不切换 |
| 动图 | GIF / WebP 动画由 mpv 逐帧渲染 |
| 自动播放 | PPT 式自动翻页，间隔 `ui.slideshow_secs` 可配 |
| 分类 | 视频/图片标签页按扩展名分栏（config.toml `media.*_extensions`） |

### 6.4 预留扩展

缩放/平移交互、浏览位置记忆（需在 mpv 子窗口之上叠加 egui 交互层或改纹理渲染方案）、
PSD/HEIC 等专用格式（mpv 不支持，需引入解码库走纹理路径）——均不影响 VFS 层。

---

## 7. 配置文件设计（config.toml，单文件集中）

```toml
codepage = "gbk"            # 非 UTF-8 字节兜底代码页（密码候选追加 + zip 老包文件名解码）
passwords = []              # 加密包密码候选列表（播放器与 VarTidy 公用，打开阶段自动探测）

[cache]   # dll 目录 / 解压缓存目录 / LRU 上限 / 解码块大小与块数 / 清理时机
[media]   # video_extensions / image_extensions
[player]  # hwdec / cache_secs / demuxer_max_mb / volume
[ui]      # font_path / slideshow_secs / wheel_switch_secs / ask_next_archive / sort_files（自然序，默认关=包内顺序）/ extract_overlay_delay_ms
[extract] # prefetch_mb（solid 包批量预取上限）/ prefetch_prev_mb（向前预取上限，见 11.1）
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
| 1 | deflate/LZMA entry 后向 seek 慢 | 解码块 LRU（必选组件）+ mpv 缓存参数调大 |
| 2 | RAR5 默认固态 | 慢路径为必选实现；解压任务支持 seek 重定向 |
| 3 | 分卷压缩（.part1.rar / .7z.001） | unrar 原生支持 RAR 分卷；ZIP 分卷需自行拼接 —— 二期 |
| 4 | 加密 | 头加密阻止列目录 → 打开阶段自动探测密码（无密码 → config 列表），耗尽才弹密码框；密码仅内存传递不落盘 |
| 5 | libmpv 嵌入渲染 | MVP 用原生子窗口嵌入（Windows `SetParent`）；纹理渲染方案后置 |
| 6 | mpv dll 分发 | 仓库不含（超 GitHub 单文件限制），build.rs 从 `deps\mpv-dev` 复制，README 提供下载指引 |
| 7 | 大图解码阻塞 UI | 已消解：图片统一走 mpv 渲染，无独立解码线程（见第 6 章） |

---

## 9. 实施路线与状态

| 阶段 | 内容 | 交付物 | 状态 |
|---|---|---|---|
| Phase 1 (MVP) | egui + libmpv + ZipVFS + 列表/搜索 + config + seek 基础链路 + 解码块 LRU | ZIP 内视频端到端可播可拖 | ✅ |
| Phase 2 | SevenzVFS / RarVFS + 固态探测 + 后台解压通路 + LRU 文件缓存 + 大包性能专项（第 11 章） | 三格式全覆盖，56G 固态包秒级定位 | ✅ |
| Phase 3 | 图片查看（mpv 统一渲染 + PPT 自动翻页）+ 密码支持（多编码候选 + 会话记忆） | 图片/动图可看，加密包可开 | ✅ |
| Phase 4 | 分卷 RAR（已支持）、分卷 ZIP、视频缩略图、播放记忆 | 完整版 | 🔶 部分完成 |
| 追加 | VarTidy CLI 子命令（第 12 章）：var 资源包增量整理 | `archview.exe tidy` + 「发送到」 | ✅ |

---

## 10. 技术选型清单（汇总）

```
ui          : egui / eframe
player      : 自研 mpv_ffi（libloading 动态加载 libmpv-2.dll，含 stream_cb）
archive/zip : zip
archive/7z  : sevenz-rust (BlockDecoder 固态直达)
archive/rar : unrar (UnRAR.dll)
image       : 无独立解码库（mpv 统一渲染）
config      : serde + toml (config.toml)
logging     : tracing
sort        : alphanumeric-sort (file list natural order, optional mode for comic/image collections)
```

---

## 11. 性能优化与虚拟列表设计（大包实战：27G ZIP / 56G 7z / 万级条目）

> 本章记录 Phase 2 大压缩包性能专项的实际优化与踩坑。核心教训：**万级条目 + GB 级包，
> 任何 O(N) 的每帧操作、任何"顺带"的全量顺序解压，都会被放大成分钟级卡顿。**

### 11.1 解压链路优化

#### 7z folder 直达（最重要的一次修复）

- **现象**：56G 固态 7z 包，解压第 417 个条目（仅一张 JPG）卡 10+ 分钟，CPU 持续满载——表象酷似死循环。
- **根因**：sevenz-rust 的 `SevenZReader::for_each_entries` **从 folder 0 开始顺序解压全部前置 folder**。
  7z 的 folder（固态块）彼此独立，每个 folder 的 pack 流偏移记录在头信息里，但该 API 不利用它。
- **修复**：改用 `BlockDecoder::new(folder_index, ...)` —— 构造时直接 seek 到目标 folder 的 pack 流偏移，
  **前置 folder 只 seek 不解压**。目标即使排在第 4 万个条目，也只需顺序解压其所在 folder 内
  目标之前的少量数据。10 分钟级 → 秒级。
- **教训**：读依赖库源码确认其遍历策略，"能跑通"的 API 路径可能隐含 O(全包) 成本。

#### 批量解压摊薄固态包定位成本（extract_batch + prefetch）

- 固态 folder 内部不可 seek，定位目标必须顺序解压一段前置数据——这部分成本无法消除，
  但可以**摊薄**：一次顺序解压流顺带解出一批条目（目标 + 顺序后续，总量 ≤ `extract.prefetch_mb`，
  默认 512MB）。翻页时直接命中缓存，无需再等。
- trait 层新增 `extract_batch(targets, cancel, progress, file_done)`：ZIP 用默认实现逐条循环；
  7z/RAR 各自实现单遍多目标（RAR 单次 processing 遍历内 skip 非目标、extract 目标）。
- 取消机制：`Arc<AtomicBool>`；批内切换目标不取消不重启，批外目标才取消重起。
  所有取消点必须打日志——否则表现为"多条后台解压日志但看不到打断"（实际是缓存命中
  路径丢弃任务却没置 cancel 标志，旧线程静默跑完整个批，与新任务并行抢 NAS 带宽）。

#### 向前预取（免费区）：随机跳看后往回翻也能命中

- 定位目标时，前置数据**必然被顺序解掉丢弃**（7z 是目标 folder 内从头解码到目标、
  RAR 是逐条 skip 消费前缀）——把这段"本来要丢的字节"改写入缓存，解码零增量，
  只增加磁盘写入。随机跳看场景下用户往回翻直接命中缓存，无需再付定位成本。
- 免费区边界由格式决定（trait `locate_start`，默认 None = 不适用）：7z = 目标所在
  folder 首条目（跨 folder 只 seek 不解压，向前预取**不得超出 folder 边界**——配置
  512MB 但 folder 前缀不足时实取以边界为准）；RAR = 整个前缀（unrar 无 folder 直达）。
- 预算独立封顶（`extract.prefetch_prev_mb`，默认 512MB，0 = 关闭），与向后预取
  `prefetch_mb` 互不挤占；仅收当前列表可见条目（目录/搜索过滤外的文件用户点不到）。
  前后批去重防 file_done 下标错位（sort_files 开启时 view 序 ≠ 物理序，两批可能重叠）。
- 进度显示配合：前置文件先于目标落地，写文件阶段字节按目标尺寸封顶
  （否则进度条出现"300MB / 60MB"怪相）；完成提示改"相邻 N 个文件"。

#### ZIP 头解析

- 中央目录（CD）逐字段偏移曾被写错（compressed/uncompressed size、name/extra/comment length
  共 6 个字段），导致 fast path 全挂、退回慢路径逐条试探——万级条目下就是"打开卡十几分钟"。
  修复后单次顺序读 CD 即完成列表（NAS 随机读 → 顺序读，分钟级 → 秒级）。
- ZIP64 支持：offset/size 超 32 位时的扩展字段解析。

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

**踩坑点**（每一条都有回归测试钉死）：

| 坑 | 现象 | 修复 |
|---|---|---|
| **usize 下溢** | 手动向后翻列表再切歌 → 列表直接跳到底部。目标行在渲染范围之前时 `pos - range.start` 下溢，debug 下 panic，**release 下 wrap 成 2^64 级天文数字** → 滚动目标被钳到 max_offset | 一切虚拟化下标换算用 `isize` |
| **行高不均匀** | 长文件名自动换行把行撑成 2 行高，首行实测高度被钉在 2 行值上 → 渲染范围按虚高缩水 → 底部空隙 + 翻页时行高双稳态翻转 | 所有列表行 `.truncate()` 强制单行；全名放视频区上方状态行 |
| **scroll_to_rect 坐标系** | 高亮追踪滚动跳到错误位置。`scroll_to_rect` 接收**屏幕坐标**，而虚拟化闭包内 `max_rect().top()` = 内容顶 + min_row×step，需按此换算目标行 y | 换算公式 `(pos - range.start) × step + 闭包 max_rect().top()` |
| **每帧重算** | 万级条目每帧过滤/排序/clone，UI 帧率跌到个位数 | 行缓存一次性构建；排序预生成小写键再按自然序比较（避免比较器内每次 2 次 to_lowercase 分配）；实测 7-Zip 打包时无视添加顺序强制按字典序写入，故 `ui.sort_files` 默认 false（包内顺序 = 固态包物理码流顺序，预取最优），true = 自然序（数字段按数值 1/2/10）供漫画/图集场景 |

**多帧测试方法论**：虚拟化 bug 大多是跨帧时序问题（滚轮惯性、动画收敛、行高反馈延迟），
单帧单 pass 的 egui 测试 (`__run_test_ui`) 复现不了。用 `ctx.run_ui(RawInput)` 手动喂
PointerMoved/MouseWheel 事件序列跑 100+ 帧，记录每帧 `ScrollArea::state.offset` 轨迹断言——
上面的 usize 下溢 bug 就是这么先复现再修复的。

### 11.3 解压进度 UX：顶栏 → 媒体区覆盖层

- 顶栏一行放不下长文件名（截断、进度条看不见）→ 进度信息移到媒体区居中覆盖层：
  完整文件名（换行）+ 两阶段进度（跳过前置文件阶段 = Spinner + 已解压字节；
  写文件阶段 = 字节进度条）。
- **防闪烁宽限期**：正在播 A 时切到需解压的 B，超过 `ui.extract_overlay_delay_ms`（默认 500ms）
  仍未解完才隐藏视频窗口（mpv 原生子窗口永远盖在 egui 之上，必须 `ShowWindow` 隐藏）显示覆盖层；
  500ms 内解完则窗口从未隐藏——快速路径零闪烁。这是"延迟隐藏"而非"最短显示时间"：
  后者会人为拖慢快速解压。
- 预取场景（目标 == 正在播放条目）永不遮挡画面，顶栏弱色短标记即可。

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

- **Archview player**：GUI 模式启动时解析命令行参数（main.rs `initial_archive`）——首个 zip/7z/rar 文件
  直接打开；目录则按名取其中首个压缩包（其余包经「目录内连续看包」N 键可达）。首帧经 `open_path` 触发，
  与拖放同链路（密码探测/弹窗/播放全复用）；非路径参数忽略，无参数启动行为不变。该入口同样覆盖
  拖到 exe 上、文件关联等一切「带路径启动」场景
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
