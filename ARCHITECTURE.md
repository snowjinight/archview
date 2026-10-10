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
| 7Z 读取 | `sevenz-rust` | 纯 Rust，支持非固态单文件解压 |
| RAR 读取 | `unrar` | 绑定官方 UnRAR 库（Windows 打包 UnRAR.dll），支持 RAR4/5、分卷 |
| 视频播放 | `libmpv-rs` | 绑定 libmpv，含 stream_cb 自定义流回调 |
| 图片解码 | `image` | JPEG/PNG/GIF/WebP/BMP + AnimationDecoder 动图 |

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
│  左侧：文件树/列表（视频+图片，类型图标）+ 搜索    │
│  右侧：媒体区 —— 按选中文件类型切换                │
│         ├─ VideoPlayerView（libmpv 子窗口嵌入）     │
│         └─ ImageViewerView（纹理渲染/缩放/平移）    │
│  顶栏：密码输入、包信息；状态栏：加载/解压/缓冲     │
└──────────────────┬──────────────────────────────────┘
                   │ 命令/事件
┌──────────────────▼──────────────────────────────────┐
│  应用核心 (状态机：浏览/加载/播放/解压中/看图中)   │
│  MediaDispatcher：按文件类型路由到 Player/Viewer    │
└────┬───────────────────┬────────────────────────────┘
     │                   │
┌────▼───────────┐  ┌────▼──────────────────────┐
│  播放服务       │  │  图片查看服务              │
│  libmpv 实例   │  │  image 解码（后台线程）    │
│  stream_cb     │  │  纹理 LRU 缓存            │
│  拉取数据      │  │  ←/→ 翻页 / 缩放平移      │
└────┬───────────┘  └────┬──────────────────────┘
     │                   │ open_entry + read
     │ Read + Seek       │
┌────▼───────────────────▼─────────────────────────────┐
│  VFS 层（核心抽象，两种消费者共用）                  │
│  trait ArchiveVFS { list(); open_entry() }           │
│  trait VirtualFile: Read + Seek { size() }           │
│  ├─ ZipVFS    （随机访问）                           │
│  ├─ SevenzVFS （探测固态；非固态随机访问）           │
│  └─ RarVFS    （探测固态；分卷合并处理）             │
└────┬─────────────────────────────────────────────────┘
     │ 固态回退：后台解压
┌────▼──────────────────┐   ┌──────────────────────────┐
│  缓存管理器            │   │  配置服务                │
│  · 解压文件缓存（慢路径）│   │  config.json 单文件：   │
│  · 解码块 LRU（快路径） │   │  缓存/扩展名/播放器参数/ │
│  · LRU 淘汰 + 上限控制  │   │  看图器参数              │
└───────────────────────┘   └──────────────────────────┘
```

**架构不变量**：播放器与图片查看器是 VFS 层的两个并列消费者，互不感知。
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

1. **mpv demuxer 缓存**：调大 `demuxer-max-bytes` / `cache-secs`（config.json 可配）。
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

## 6. 图片预览模块（类 HoneyView）

### 6.1 定位

与视频播放并列的 VFS 消费者。扫描阶段按扩展名将 entry 分为视频/图片两类
（分类规则集中在 config.json），列表以图标区分，双击/回车打开。

### 6.2 数据流

```
选中图片 → open_entry() → 后台线程全量读入 + image 解码
        → 上传 egui 纹理（后台完成后通知 UI）
        → 纹理进入 LRU 缓存（上限可配，默认 32 张）→ ←/→ 翻页秒切
```

### 6.3 功能与交互

| 功能 | 说明 |
|---|---|
| 缩放 | 滚轮缩放；模式：适应窗口 / 1:1 / 自由倍率（可配默认值） |
| 平移 | 放大后拖拽平移 |
| 翻页 | ←/→ 或 PgUp/PgDn 在当前文件列表内切换；预解码相邻 1~2 张 |
| 动图 | GIF / WebP 动画通过 image 的 AnimationDecoder 帧动画渲染 |
| 大图 | 后台解码 + loading 占位，不阻塞 UI |
| 记忆 | 记住同包内上次浏览位置与缩放模式 |

### 6.4 格式支持

- 一期（`image` crate 原生）：JPEG / PNG / GIF / WebP / BMP / TIFF
- 二期（按需）：PSD、HEIC 等引入专用解码库；不影响 VFS 层

### 6.5 预留扩展

同一消费者模式可继续接入：音频播放、视频缩略图（mpv 截首帧）、文本/字幕预览等。

---

## 7. 配置文件设计（config.json，单文件集中）

```json
{
  "cache": {
    "dir": "auto",
    "max_size_mb": 2048,
    "chunk_size_mb": 4,
    "chunk_cache_blocks": 64,
    "texture_cache_count": 32
  },
  "media": {
    "video_extensions": ["mp4", "mkv", "avi", "mov", "flv", "ts", "webm", "wmv", "m4v"],
    "image_extensions": ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"],
    "player": {
      "hwdec": "auto",
      "cache_secs": 30,
      "demuxer_max_mb": 512
    }
  },
  "viewer": {
    "background": "#1e1e1e",
    "default_zoom": "fit",
    "preload_neighbors": 2
  }
}
```

> 约定：所有路径、阈值、扩展名列表、播放器参数均只在此文件配置，代码零硬编码。

---

## 8. 风险点

| # | 风险 | 对策 |
|---|---|---|
| 1 | deflate/LZMA entry 后向 seek 慢 | 解码块 LRU（必选组件）+ mpv 缓存参数调大 |
| 2 | RAR5 默认固态 | 慢路径为必选实现；解压任务支持 seek 重定向 |
| 3 | 分卷压缩（.part1.rar / .7z.001） | unrar 原生支持 RAR 分卷；ZIP 分卷需自行拼接 —— 二期 |
| 4 | 加密 | 头加密阻止列目录 → 先弹密码框；密码仅内存传递不落盘 |
| 5 | libmpv 嵌入渲染 | MVP 用原生子窗口嵌入（Windows `SetParent`）；纹理渲染方案后置 |
| 6 | UnRAR.dll / mpv dll 分发 | 随程序打包 x64 dll；构建脚本从 config 指定的目录复制 |
| 7 | 大图解码阻塞 UI | 解码全部在后台线程，完成后经 channel 通知 UI |

---

## 9. 实施路线

| 阶段 | 内容 | 交付物 |
|---|---|---|
| Phase 1 (MVP) | egui + libmpv + ZipVFS + 列表/搜索 + config.json + seek 基础链路 + 解码块 LRU | ZIP 内视频端到端可播可拖 |
| Phase 2 | SevenzVFS / RarVFS + 固态探测 + 后台解压通路 + 解压任务 seek 重定向 + LRU 文件缓存 | 三格式全覆盖 |
| Phase 3 | 图片查看器（解码/缩放/翻页/动图）+ 密码支持 | 类 HoneyView 体验 |
| Phase 4 | 分卷、视频缩略图、播放记忆、PSD/HEIC | 完整版 |

> 注：图片查看器架构自 Phase 1 起即预留接口（MediaDispatcher + VFS 消费者模式），实现排在 Phase 3。

---

## 10. 技术选型清单（汇总）

```
ui          : egui / eframe
player      : libmpv-rs (stream_cb)
archive/zip : zip
archive/7z  : sevenz-rust
archive/rar : unrar (UnRAR.dll)
image       : image
serde       : serde + serde_json (config.json)
logging     : tracing
```
