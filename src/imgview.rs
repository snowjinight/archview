//! 图片纹理直显服务（doc/架构设计/13_渲染架构迁移.md §13.4 第 1-4 步）：
//! 静态图与动图（gif / webp 动画）均不再经 mpv，后台线程 image crate 解码
//! → egui 纹理 → UV 变换绘制；动图按帧延迟定时 repaint 逐帧播放。
//!
//! 数据流：`App::play` 路由判定（[`wants_texture`]）→ [`ImageService::request`]
//! spawn 解码线程（zip/dir 用 VFS 虚拟文件读取；7z/RAR 读已落盘缓存）→
//! 降采样/上采样 → mpsc 回 UI 线程 → [`ImageService::poll`] 上传纹理进 LRU。
//!
//! - 尺寸适配两档（见 [`resample`]）：基础档 = 原图×ppp 预放大（阈值型，
//!   native×ppp ≤ `image_max_dim` 才升，翻页即 100% 密度直出；超限原尺寸
//!   直进）；full 高清档 = 原图×ppp×3（clamp 型，封顶 [`FULL_MAX_DIM`] +
//!   像素预算 [`FULL_MAX_PIXELS`]，深放大按需二次解码）。病态图（>8192）
//!   一律降采样兜底。full 的原图源优先复用 base worker 留存的单槽缓存
//!   （免二次解码），无缓存才从源重新解码
//! - 重采样统一走 fast_image_resize 的 SIMD Lanczos3（与 image crate 同核，
//!   SSE4/AVX2 提速 ~10 倍，33MP 级 600ms→~80ms）
//! - 动图：gif 经 `AnimationDecoder::into_frames` 全帧预解码（单帧 gif 解出
//!   1 帧自动归静态；动图不做预放大——多帧 ×factor² 爆显存）。帧延迟 < 20ms
//!   按浏览器惯例钳到 100ms（坏帧疯闪）；帧数封顶 [`MAX_ANIM_FRAMES`] 防内存
//!   失控。逐帧纹理 + `tick` 推进，UI 每帧取当前帧并
//!   `request_repaint_after(帧延迟)` 驱动播放。
//! - webp：`has_animation()`（VP8X 头 ANIM 位）先验分流——动画走 gif 同款
//!   逐帧解码；静态走单帧 decode（image-webp 静态文件 `num_frames()==0`，
//!   `into_frames` 解出空集，实测踩坑，不可盲目逐帧）。
//! - 失败：解码错误经 [`ImgEvent::Failed`] 回传，UI 回退 mpv 播放该条目。
//!
//! 缩放/平移的纯几何函数（[`fit_scale`]/[`zoom_pan`]/[`clamp_pan`]）也放在这里，
//! 便于单测与 UI 层复用。

use crate::iosched::{IoClass, IoScheduler};
use crate::vfs::{ArchiveVFS, VirtualEntry};
use egui::TextureOptions;
use image::{AnimationDecoder, GenericImageView};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read, Seek};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 动图帧数封顶：动图全帧驻留显存（每帧 = 宽×高×4 字节），超限截断防失控
/// （500 帧长图罕见；截断后循环播放缺失尾段，优于解码失败回退 mpv）
const MAX_ANIM_FRAMES: usize = 512;

/// 解码看门狗超时：正常解码（含大图、GIF 多帧、冷盘 IO）远小于此；
/// 超过视为线程卡死或消息丢失，剔除 pending 允许重新发起
pub(crate) const PENDING_TIMEOUT: Duration = Duration::from_secs(10);

/// 病态图两档共用兜底：最长边超过一律降采样（LRU/动图全驻显存，
/// 8192²×4B=256MB 封顶，防 30000×30000 级病图撑爆）
const MAX_TEX_DIM: u32 = 8192;

/// full 槽纹理最长边上限：full 资格图 native ≤ image_max_dim(4096)/ppp，
/// ×ppp×3 后最长边 ≤ 12288 恰被此值覆盖；4K 级原图 clamp 到 ×3.2
pub const FULL_MAX_DIM: u32 = 12288;

/// full 槽总像素预算（二次封顶）：最长边封顶后正方形极端图（12288²≈151MP）
/// 仍会超表，按总像素等比缩回。100MP×4B = 400MB 峰值显存兜底
pub const FULL_MAX_PIXELS: u64 = 100_000_000;

/// 缓存键 = 压缩包 id + 条目下标（目录模式 id 同样有效，见 ArchiveState）
pub type ImgKey = (u64, usize);

/// 纹理上传参数：双线性 + mipmap（仅 glow 后端支持 mipmap，本项目即 glow）。
/// 缩小超过一倍时 mipmap 消除摩尔纹/闪烁，放大时双线性平滑
pub const TEX_OPTS: TextureOptions = TextureOptions {
    magnification: egui::TextureFilter::Linear,
    minification: egui::TextureFilter::Linear,
    wrap_mode: egui::TextureWrapMode::ClampToEdge,
    mipmap_mode: Some(egui::TextureFilter::Linear),
};

/// full 档纹理上传参数：双线性、**无 mipmap**。full 的甜点在 badge 300%
/// （默认阈值 2.0 从 badge 200% 起接管，badge 300% 处采样比 = 1 逐纹素
/// 直出）——若开 mipmap，缩小带的半分辨率 mip1 会让 3× Lanczos 的锐度
/// 增益打折；无 mipmap 的双线性在 ≥0.67× 缩小下仅轻微欠采样，静止图
/// 无感。base 档保留 mipmap（fit 态大图深度缩小仍需要防摩尔纹）
pub const TEX_OPTS_FULL: TextureOptions = TextureOptions {
    magnification: egui::TextureFilter::Linear,
    minification: egui::TextureFilter::Linear,
    wrap_mode: egui::TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};

/// 缩放限制：下限足够小（贴纸/小图标也能看），上限 ~25600%（看图软件量级）
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 256.0;

/// 图片视图状态：zoom=None=适应窗口，Some(z)=相对基础纹理像素的倍率
/// （显示物理尺寸 = 纹理尺寸×z×ppp；"100%"落点 = 1/ppp，纹素:物理像素
/// 1:1 零重采样，见 panels.rs 双击处理）。
/// pan = 图像中心相对视口中心的偏移（像素）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImgView {
    pub zoom: Option<f32>,
    pub pan: egui::Vec2,
}

/// 该条目是否走纹理直显 + 配置开关。
/// 静态图与动图（gif / 动画 webp）都走：由 image crate `into_frames`
/// 逐帧解码、egui 定时 repaint 播放（13.4 第 4 步）。
/// mpv 只保留视频职责与解码失败兜底
pub fn wants_texture(cfg: &crate::config::Config, name: &str) -> bool {
    cfg.image.image_texture && cfg.is_image_ext(name)
}

/// 图片数据来源
#[derive(Clone)]
pub enum ImageSource {
    /// zip/目录：流式 open_entry 读虚拟文件解码（不解压落盘）
    Stream(Arc<dyn ArchiveVFS>, VirtualEntry),
    /// 7z/RAR：读 extract 会话已落盘的缓存文件
    File(PathBuf),
}

/// UI 线程每帧 poll 到的事件
pub enum ImgEvent {
    /// 解码完成、纹理已上传进缓存
    Ready(ImgKey),
    /// 解码失败（key = 原请求，UI 据此回退 mpv）
    Failed(ImgKey, String),
}

/// 解码结果：静态单帧 / 动图多帧（帧 + 显示时长，顺序即播放顺序）
pub enum Decoded {
    Static(egui::ColorImage),
    Anim(Vec<(egui::ColorImage, Duration)>),
}

/// 直方图：R/G/B/亮度四通道 256 bin。解码 worker 在拿到 CPU 像素时
/// 顺手计算（O(像素) 一遍，被解码耗时掩盖），随纹理一起进 LRU。
/// 位深语义：标准 8bit 图直接计 bin；raw 由软解归一（bit_shift 左移 +
/// 转换器取高 8 位），净效果 = 全量程均匀压缩到 0..255，无高位舍弃
#[derive(Debug, PartialEq)]
pub struct Histogram {
    /// [R, G, B, 亮度] 各 256 bin
    pub ch: [[u32; 256]; 4],
}

impl Histogram {
    /// 从解码后像素统计。ColorImage 存储为预乘 alpha（from_rgba_unmultiplied
    /// 内部预乘），半透明像素先去预乘还原原色——不透明图（照片主流）零开销路径
    pub fn compute(img: &egui::ColorImage) -> Self {
        let mut h = Self { ch: [[0; 256]; 4] };
        for px in &img.pixels {
            let (r, g, b, a) = (px.r() as u32, px.g() as u32, px.b() as u32, px.a() as u32);
            let (r, g, b) = if a < 255 && a > 0 {
                (r * 255 / a, g * 255 / a, b * 255 / a)
            } else {
                (r, g, b)
            };
            h.ch[0][(r as usize).min(255)] += 1;
            h.ch[1][(g as usize).min(255)] += 1;
            h.ch[2][(b as usize).min(255)] += 1;
            // 整数 BT.601 亮度：0.299R + 0.587G + 0.114B
            let y = (r * 299 + g * 587 + b * 114) / 1000;
            h.ch[3][(y as usize).min(255)] += 1;
        }
        h
    }

    /// 四通道全局最大 bin（归一化用；空图返回 1 防除零）
    pub fn max_bin(&self) -> u32 {
        self.ch
            .iter()
            .flat_map(|c| c.iter())
            .copied()
            .max()
            .unwrap_or(1)
            .max(1)
    }
}

enum ImgMsg {
    Ready {
        key: ImgKey,
        decoded: Decoded,
        /// 直方图（静态图全图 / 动图首帧；解码失败时不产生）
        hist: Option<Histogram>,
        /// 文件短名：仅供上传日志标注
        name: String,
        /// 原图保留（仅静态且具备 full 资格的图：native×factor ≤ max_dim），
        /// 供 full 升级复用免二次解码；动图/超大图为 None
        native: Option<image::DynamicImage>,
        /// 本次解码实际应用的 EXIF 方向值（静态图；动图恒 1）——
        /// 进 ImageService.orientations 方向事实表
        orient: u8,
        /// 发牌类（调度器记账回执）：pending 可能在消息前被看门狗/invalidate
        /// 移除，类随消息走保证 complete 恒可记账
        class: IoClass,
        /// 发起时的 base 解码代次：poll 验证 pending 内代次相符才落缓存
        /// ——invalidate/看门狗剔除后重发的请求，旧 worker 迟到产物（旧
        /// 配置纹理）按代次丢弃（与 FullReady.epoch 同口径）
        gen: u64,
    },
    Failed {
        key: ImgKey,
        error: String,
        /// 发牌类（同 Ready.class）
        class: IoClass,
        /// 发起时的 base 解码代次（同 Ready.gen）：poll 验证代次相符才
        /// remove pending——旧 worker 迟到失败不吃掉新代次请求的占位
        gen: u64,
    },
    /// full 槽纹理就绪（原图 ×ppp×3，见 request_full）。epoch = 发起时的
    /// full 请求代次：被 invalidate 丢弃的旧 worker 迟到产物按代次丢弃。
    /// io = 本次是否做了源读（原图缓存命中 = 纯 CPU 重采样零 IO，发牌方
    /// 未占槽，回账按此标志对齐）
    FullReady {
        key: ImgKey,
        img: egui::ColorImage,
        name: String,
        epoch: u64,
        io: bool,
    },
    /// 原图解码失败/放弃：UI 保持降采样纹理（io 同 FullReady）
    FullFailed {
        key: ImgKey,
        epoch: u64,
        io: bool,
    },
}

/// tex-lru 缓存条目：逐帧纹理 + 播放状态（静态图 = 单帧无时钟）
struct Cached {
    /// 帧纹理（静态图长度 1；动图长度 = 帧数）
    frames: Vec<egui::TextureHandle>,
    /// 各帧显示时长，与 frames 对齐（静态图 = [ZERO]）
    delays: Vec<Duration>,
    /// 当前播放帧下标
    cur: usize,
    /// 下次切帧时刻（动图专用；None = 尚未开始/静态图无时钟）
    next_at: Option<Instant>,
    /// 直方图（静态图 = 全图；动图 = 首帧），与纹理同生命周期进 LRU
    hist: Option<Histogram>,
}

/// deferred 请求种类：base 档按类发牌；full 高清档恒 Decode 类
#[derive(Clone)]
enum DeferredKind {
    Base { max_dim: u32, class: IoClass },
    Full { sharpen: SharpenCfg },
}

/// deferred 队列条目：admission 拒绝的请求原样排队（FIFO 保序 = 单图
/// 串行链"当前张 → +1 → −1"顺序涌现），poll 完成事件驱动逐个放行
#[derive(Clone)]
struct DeferredReq {
    key: ImgKey,
    source: ImageSource,
    name: String,
    kind: DeferredKind,
    /// 入队时刻（放行日志打等待时长）
    at: Instant,
}

pub struct ImageService {
    /// 统一 IO 发牌调度器（App 构造共享；发牌/回执只在 UI 线程）
    sched: Arc<IoScheduler>,
    tx: Sender<ImgMsg>,
    rx: Receiver<ImgMsg>,
    ctx: egui::Context,
    /// 解码中集合：发起时刻 + 发牌类 + 代次 → 看门狗超时判定与完成记账。
    /// 防重复 spawn 同一条目（请求 → 线程 → Ready/Failed 才移除）。代次
    /// 供 poll Ready 验证：被 invalidate/看门狗剔除后按新代次重发的请求，
    /// 旧 worker 迟到产物按代次丢弃（与 full_epoch 同口径）
    pending: HashMap<ImgKey, (Instant, IoClass, u64)>,
    /// base 解码请求代次（单调递增）：spawn_base 捕获、Ready 回传比对
    base_gen: u64,
    /// admission 拒绝的请求队列（FIFO）：完成事件驱动逐个放行
    deferred: VecDeque<DeferredReq>,
    /// **per-file 显示对比度**（W/S 快捷键，值 = ×1.25 步数 n）：App 共享
    /// 同一 Rc<RefCell>（单源真值，视频属性/图片 LUT/对比 raw 重放都读
    /// 这里）。poll 上传期应用 LUT——解码发起在先、调对比度在后也恒拿到
    /// 最新值，无 worker 竞态
    contrast_map: std::rc::Rc<std::cell::RefCell<HashMap<ImgKey, i32>>>,
    /// **tex-lru**：GPU 纹理缓存 LRU（front = 最近使用；back = 最久未用，
    /// 逐出候选）。全图的翻页缓存——条目内容 = base 档显示纹理（GPU 显存
    /// TextureHandle）：小图 = native×ppp 预放大、大图 = native 直出、
    /// 超大图（>8192）= 降采样缩略；±1 邻居预解码结果也进这里。容量 =
    /// image_cache_count（默认 3）。解码线程产出的 CPU 像素在此上传 GPU
    /// 后即丢弃，CPU 侧不驻留 ×ppp 中间产物
    cache: VecDeque<(ImgKey, Cached)>,
    cap: usize,
    /// 最近一次成功显示的条目：逐出保护。滚轮连翻时每跳 ±1 预取的推入
    /// 洪水会把正在显示的条目从队尾挤出（cap 小时必现），导致该图
    /// frame()=None 而 pending 已空 → 永久停在"正在解码图片…"占位符
    shown: Option<ImgKey>,
    /// 对比模式显示集合：多栏同时显示时全部受逐出保护（预取洪水不挤掉
    /// 任何一栏）；单图模式恒空，与 shown 互不干扰
    shown_multi: Vec<ImgKey>,
    /// **full-slot**：GPU full 高清纹理单槽（非 LRU）：大图放大到 1:1 附近
    /// 时按需解码不降采样版本无缝换画。单槽不进 LRU——原图显存大（12288
    /// 最长边 + 100MP 预算，峰值 ~400MB），只随当前显示条目存续
    /// （frame() 切条目即弃）
    full: Option<(ImgKey, egui::TextureHandle)>,
    /// 原图解码中（key + 发起时刻，后者供看门狗超时判定）
    /// full 单槽在途：(key, 发起时刻, 是否做源读)。第三项 = 记账对齐
    /// （原图缓存命中的 full 是纯 CPU 零 IO，不占槽不回账）
    full_pending: Option<(ImgKey, Instant, bool)>,
    /// full 请求代次：invalidate 丢弃在途 full worker 时递增——迟到消息按
    /// 代次判旧（旧 worker 产物是旧解码/增强配置，不能顶掉新参数请求）
    full_epoch: u64,
    /// 原图解码放弃标记（失败/超时/动图）：防止每帧重触发无限重解
    full_failed: Option<ImgKey>,
    /// **orig-lru**：CPU 原图缓存 LRU（base worker 顺手留存，静态图按
    /// orig-lru 准入判据）：三个消费者——① full 升级复用免二次熵解码；
    /// ② alt+点击像素探针取**原图**像素值（显示纹理是插值重采样结果，
    /// 取样会拿到缩放插值而非真实像素，见 [`ImageService::original_pixel`]；
    /// 预算制后任意准入尺寸都有读数）；③ per-file 对比度 W/S native 重建
    /// （免重解码，毫秒级）。张数 cap 随当前栏数自适应（当前栏数 + 2，
    /// 单图 3、对比 n 栏 n+2）+ 字节预算（config image.image_orig_cache_mb）
    /// 双护栏逐出最旧（当前显示受保护），随翻页自然更替。**不与 tex-lru
    /// 重复**——持有的是原始分辨率 CPU 像素
    natives: VecDeque<(ImgKey, image::DynamicImage)>,
    /// orig-lru 字节预算（config image.image_orig_cache_mb）：natives 当前
    /// 驻留字节总数随推入/逐出维护，超预算逐出最旧（当前显示受保护）
    orig_budget: usize,
    /// natives 当前驻留字节总数（as_bytes().len() 增量维护）
    natives_bytes: usize,
    /// 方向事实表：各 key 最近一次解码应用的 EXIF 方向值（静态图；动图
    /// 不记录）。R 键旋转在此值上轮转；写回路径写完文件后立即同步更新
    orientations: HashMap<ImgKey, u8>,
    /// 方向覆盖表：不可写格式（GIF/BMP/lossy webp/压缩包内/RAW）的 R 键
    /// 旋转只存会话内存——request 时下发给 worker 覆盖文件 EXIF 方向。
    /// 可写格式不走此表（直接写回文件）
    rotoverride: HashMap<ImgKey, u8>,
}

impl ImageService {
    pub fn new(
        ctx: egui::Context,
        cap: usize,
        contrast_map: std::rc::Rc<std::cell::RefCell<HashMap<ImgKey, i32>>>,
        orig_budget: usize,
        sched: Arc<IoScheduler>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            sched,
            tx,
            rx,
            ctx,
            contrast_map,
            pending: HashMap::new(),
            base_gen: 0,
            deferred: VecDeque::new(),
            cache: VecDeque::new(),
            cap: cap.max(1),
            shown: None,
            shown_multi: Vec::new(),
            full: None,
            full_pending: None,
            full_epoch: 0,
            full_failed: None,
            natives: VecDeque::new(),
            orig_budget: orig_budget.max(1),
            natives_bytes: 0,
            orientations: HashMap::new(),
            rotoverride: HashMap::new(),
        }
    }

    /// 更新 LRU 容量（config 热重载同步 ui.image_cache_count）。
    /// 收缩在下次推入时生效（逐出按本字段比较，已缓存内容不主动丢弃）
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
    }

    /// 更新 orig-lru 字节预算（config 热重载同步 image.image_orig_cache_mb）。
    /// 收缩在下次推入时生效
    pub fn set_orig_budget(&mut self, mb: u32) {
        self.orig_budget = (mb.clamp(64, 4096) as usize) * 1024 * 1024;
    }

    /// 丢弃指定条目的显示缓存（config 热重载后当前播放项重放用）：tex 条目
    /// 与 full 三态清掉，下次 request 走全新解码（新配置生效）。natives 保留
    /// ——原图像素与解码配置无关，探针 / 对比度重建继续可用
    pub fn invalidate(&mut self, key: ImgKey) {
        if let Some(i) = self.pos(key) {
            self.cache.remove(i);
        }
        self.pending.remove(&key);
        // deferred 同 key 条目同步清掉：被 invalidate 的请求不再放行
        // （热重载重放/旋转写回后旧参数请求无意义）
        self.deferred.retain(|d| d.key != key);
        // 在途 full worker 同 key：清 pending 并递增代次——迟到的 FullReady/
        // FullFailed 按代次丢弃（其产物是旧配置参数，不能顶掉 invalidate 后
        // 的新参数请求）
        if self.full.as_ref().is_some_and(|(k, _)| *k == key) {
            self.full = None;
        }
        if self.full_pending.as_ref().is_some_and(|(k, _, _)| *k == key) {
            self.full_pending = None;
            self.full_epoch += 1;
        }
        if self.full_failed.as_ref() == Some(&key) {
            self.full_failed = None;
        }
    }

    /// 丢弃指定条目全部缓存**含 natives 原图**（旋转/评级写回后专用）：
    /// 写回改变了文件内容语义（新 EXIF 方向），旧原图像素（已按旧方向
    /// 矫正）不能再供探针/full 复用/对比度重建——三消费者都会用过期像素。
    /// 全部清掉，下次 request 走全新解码
    pub fn invalidate_full(&mut self, key: ImgKey) {
        self.invalidate(key);
        if let Some(pos) = self.natives.iter().position(|(k, _)| *k == key) {
            if let Some((_, old)) = self.natives.remove(pos) {
                self.natives_bytes -= old.as_bytes().len();
            }
        }
    }

    /// 方向事实读取（R 键轮转基准）：无记录 = 1（未矫正）
    pub fn orientation(&self, key: ImgKey) -> u8 {
        self.orientations.get(&key).copied().unwrap_or(1)
    }

    /// 方向事实写入（R 键写回成功后立即同步，防 full 档在 base 重解码
    /// 完成前用旧方向二次解码出错误朝向）
    pub fn record_orientation(&mut self, key: ImgKey, o: u8) {
        self.orientations.insert(key, o);
    }

    /// 会话内方向覆盖（不可写格式）：request 时下发 worker 覆盖文件 tag
    pub fn set_orient_override(&mut self, key: ImgKey, o: u8) {
        self.rotoverride.insert(key, o);
    }

    /// 清除方向覆盖（可写格式写回文件后调用——文件 tag 已是新值，
    /// 覆盖残留会让后续解码跳过文件读取走旧覆盖值）
    pub fn clear_orient_override(&mut self, key: ImgKey) {
        self.rotoverride.remove(&key);
    }

    /// 对比模式显示集合（逐出保护；退出对比时清空）
    pub fn set_shown_multi(&mut self, keys: Vec<ImgKey>) {
        self.shown_multi = keys;
    }

    pub fn clear_shown_multi(&mut self) {
        self.shown_multi.clear();
    }

    /// 请求解码（幂等：已缓存/解码中直接返回）。
    /// 调用方负责构造 source；max_dim 来自 cfg.image.image_max_dim；
    /// name 仅用于日志（定位"发起了没/何时发起"）；
    /// class = 发牌类（当前显示 Decode / 后台预取 Prefetch）。
    /// admission 通过即 spawn；拒绝挂 deferred 队列，完成事件驱动放行
    pub fn request(
        &mut self,
        key: ImgKey,
        source: ImageSource,
        max_dim: u32,
        name: &str,
        class: IoClass,
    ) {
        if self.pending.contains_key(&key) || self.pos(key).is_some() {
            return;
        }
        if self.sched.admission(class) {
            self.spawn_base(key, source, max_dim, name, class);
        } else {
            // IO 等待关键转折（info）：谁在排队、为什么（各类在途计数）
            tracing::info!(
                "IO 排队 {class:?} {name}（decode={} prefetch={} rating={}，队列 {}）",
                self.sched.decode_out(),
                self.sched.prefetch_out(),
                self.sched.rating_out(),
                self.deferred.len() + 1
            );
            self.deferred.push_back(DeferredReq {
                key,
                source,
                name: name.to_owned(),
                kind: DeferredKind::Base { max_dim, class },
                at: Instant::now(),
            });
        }
    }

    /// 发牌开工：记账 + 插 pending + spawn 解码线程（request/deferred 放行
    /// 共用；spawn 失败即时回账，不会等到看门狗）
    fn spawn_base(
        &mut self,
        key: ImgKey,
        source: ImageSource,
        max_dim: u32,
        name: &str,
        class: IoClass,
    ) {
        // 基础档预放大倍率 = 屏幕 DPI（ppp）：150% 缩放屏 ×1.5。低密屏
        // （ppp<1）钳 1.0 免无谓缩小重采样。取发起时刻值；跨屏迁移后
        // 已缓存纹理不回炉（LRU 深度浅，翻几页即换血）
        let factor = (self.ctx.pixels_per_point() as f64).max(1.0);
        // orig-lru 单张准入 = 预算/2：防单张巨图独占预算（当前显示受保护
        // 的那张最坏驻留预算一半，其余合计不超另一半）
        let native_limit = (self.orig_budget / 2) as u64;
        // 会话内方向覆盖（R 键旋转不可写格式路径）：request 时快照下发，
        // worker 覆盖文件 EXIF 方向
        let orient_override = self.rotoverride.get(&key).copied();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let name = name.to_owned();
        self.base_gen += 1;
        let gen = self.base_gen;
        self.pending.insert(key, (Instant::now(), class, gen));
        self.sched.issue(class);
        // 开工细节（debug）：直接发牌与放行共用此入口，info 级转折在
        // 「IO 排队 / IO 放行」两处
        tracing::debug!(
            "IO 开工 {class:?} {name}（decode={} prefetch={}）",
            self.sched.decode_out(),
            self.sched.prefetch_out()
        );
        let r = std::thread::Builder::new()
            .name("imgdec".into())
            .spawn(move || {
                // 解码对畸形文件可能 panic（坏图触发 codec 内部越界等）：
                // 捕获转 Failed。否则线程静默死亡（无 send、无 repaint），
                // pending 永久泄漏 → 该图被幂等检查拦截 → 永久占位死局
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_image_orient(
                        &source,
                        max_dim,
                        false,
                        factor,
                        &name,
                        &SharpenCfg::OFF,
                        native_limit,
                        orient_override,
                    )
                }));
                match r {
                    Ok(Ok((decoded, native, orient))) => {
                        // 直方图在解码线程顺手算（O(像素)，被解码耗时掩盖）：
                        // 静态图全图；动图取首帧（逐帧统计显存外的 CPU 成本
                        // 随帧数线性涨，且直方图语义是"这张图长什么样"）
                        let hist = match &decoded {
                            Decoded::Static(img) => Some(Histogram::compute(img)),
                            Decoded::Anim(a) => a.first().map(|(f, _)| Histogram::compute(f)),
                        };
                        let _ = tx.send(ImgMsg::Ready {
                            key,
                            decoded,
                            hist,
                            name,
                            native,
                            orient,
                            class,
                            gen,
                        });
                    }
                    Ok(Err(e)) => {
                        let _ = tx.send(ImgMsg::Failed { key, error: e, class, gen });
                    }
                    Err(_) => {
                        let _ = tx.send(ImgMsg::Failed {
                            key,
                            error: "解码线程 panic".into(),
                            class,
                            gen,
                        });
                    }
                }
                // 唤醒 UI 立即 poll：否则要等下一次重绘（空闲时可迟 100ms+，
                // 实测 LOG 解码完成→上帧间隔 50~130ms 全是这个）
                ctx.request_repaint();
            });
        if r.is_err() {
            // 线程起不来（极端）：移除 pending + 回账，让后续请求可重试
            self.pending.remove(&key);
            self.sched.complete(class);
        }
    }

    /// 请求解码高清纹理（full 单槽，clamp FULL_MAX_DIM + 像素预算
    /// FULL_MAX_PIXELS）：深放大按需升级，原图 Lanczos ×(ppp×3)（150%
    /// 缩放屏 ×4.5，badge 300% 处 1:1 采样）——绝不在基础纹理（已 ×ppp）
    /// 上级联再放大（两级重采样抹锐边）。倍率内部按屏幕 DPI 取值，调用方
    /// 无需传参。
    /// full 高清档派发（request_full / deferred 放行共用）：**先查原图
    /// 缓存再决定发牌**——natives 命中 = 纯 CPU 重采样（fast_image_resize
    /// + 锐化），零 IO 不占槽直接开工（full 升级大多数情况走此路径，用
    /// 户洞察：占着 IO 槽排队纯属错门）；未命中 = 真源读，按 Decode 发牌。
    /// 返回 false = 槽位忙需排队（request 入 deferred；放行 loop 保持
    /// 队头）。已就绪/在途/已放弃返回 true（无事可做）
    fn dispatch_full(
        &mut self,
        key: ImgKey,
        source: ImageSource,
        name: &str,
        sharpen: SharpenCfg,
    ) -> bool {
        if self.has_full(key) || self.is_full_pending(key) || self.is_full_failed(key) {
            return true;
        }
        // full 单槽互斥：其它条目的 full 在途时不抢槽（返回 true = 调用方
        // 认为已受理；max≥2 时槽位算术放得行，双 worker 并发的旧产物会被
        // epoch/key 检查丢弃白费一路 IO，且覆盖 full_pending 单槽记账）。
        // 触发方每帧重触发/deferred 保持等待，槽腾出后自然开工
        if self.full_pending.is_some() {
            return true;
        }
        // 原图缓存命中即复用（clone 一份：natives 同时服务像素探针不能
        // take；缓冲拷贝 ~10ms 后台线程无感）；未命中走二次解码（真 IO）
        let cached = self
            .natives
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, n)| n.clone());
        if cached.is_none() && !self.sched.admission(IoClass::Decode) {
            return false;
        }
        self.spawn_full(key, source, name, sharpen, cached);
        true
    }

    /// 原图源：base worker 留存的原图缓存命中即复用（免二次解码，2K 图省
    /// ~300ms，**且零 IO——不占 IO 槽**）；未命中（切图回跳/缓存被换）从源
    /// 重新解码（真 IO，按 Decode 发牌，槽忙挂 deferred 完成事件驱动放行）。
    /// 升级资格由触发方保证：仅 native ≤ image_max_dim 的图（即基础
    /// 纹理走了预放大）值得升级；超大图原尺寸直进 LRU、深放大交给 GPU
    /// 双线性（二次解码秒级 + 300MB 级显存，增益 <×1.4 不值当）。
    /// 就绪后无缝换画
    /// （几何不变：屏幕矩形按基础纹理算，UV 全图映射对两种分辨率通用）。
    /// 幂等：已就绪/解码中/已放弃直接返回；动图不升级（全帧原图显存
    /// 不可控，触发方已用 is_static 前置过滤，单帧动图落到这里按
    /// FullFailed 放弃即可）
    /// sharpen 直传 config image.image_full_sharpen*（默认开+0.6+σ1.0+降噪）：
    /// 放大前在原图分辨率执行（观感增强，成本 ~50ms，见 sharpen_full）
    pub fn request_full(
        &mut self,
        key: ImgKey,
        source: ImageSource,
        name: &str,
        sharpen: SharpenCfg,
    ) {
        if !self.dispatch_full(key, source.clone(), name, sharpen) {
            // IO 等待关键转折（info）：真源读的 full 在排队（缓存命中的
            // 纯 CPU full 不会到这里）
            tracing::info!(
                "IO 排队 Decode(full) {name}（decode={} prefetch={} rating={}，队列 {}）",
                self.sched.decode_out(),
                self.sched.prefetch_out(),
                self.sched.rating_out(),
                self.deferred.len() + 1
            );
            self.deferred.push_back(DeferredReq {
                key,
                source,
                name: name.to_owned(),
                kind: DeferredKind::Full { sharpen },
                at: Instant::now(),
            });
        }
    }

    /// 发牌开工（full 高清档）：单槽占位 + spawn（dispatch_full / deferred
    /// 放行共用；spawn 失败即时回账）。cached = 原图缓存命中（调用方查好
    /// 传入）：命中 = 纯 CPU 重采样零 IO，**不占槽不发牌**；未命中 = 真源
    /// 读，issue 记账（FullReady/FullFailed/看门狗按 io 标志回账）
    fn spawn_full(
        &mut self,
        key: ImgKey,
        source: ImageSource,
        name: &str,
        sharpen: SharpenCfg,
        cached: Option<image::DynamicImage>,
    ) {
        tracing::debug!("发起高清解码 {name}（{}）", if cached.is_some() { "原图缓存复用" } else { "源读" });
        // 高清档倍率 = ppp×3：badge（zoom×ppp）300% 处 full 纹素与物理像素
        // 1:1，300%~400% 主用区间锐度完整；200%~300% 为 0.67~1× 轻微缩采样
        // （静止图无感）。默认阈值 2.0（badge 200% 滚轮档）起接管
        let factor = (self.ctx.pixels_per_point() as f64).max(1.0) * 3.0;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let name = name.to_owned();
        let epoch = self.full_epoch;
        let did_io = cached.is_none();
        self.full_pending = Some((key, Instant::now(), did_io));
        if did_io {
            self.sched.issue(IoClass::Decode);
        }
        // 方向事实快照：full 档从源重解码时必须与 base 档同朝向（R 键
        // 写回路径已同步此表，见 record_orientation）
        let orient = self.orientation(key);
        let r = std::thread::Builder::new()
            .name("imgfull".into())
            .spawn(move || {
                // 同 request：畸形文件可能 panic，捕获转 FullFailed 防死局
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match cached {
                    Some(img) => {
                        tracing::debug!("高清解码复用原图缓存{} {name}", sharpen.tag());
                        Ok(build_full(img, factor, &sharpen))
                    }
                    None => {
                        // full 档：原图即输入不保留 native（native_limit 传 0）。
                        // 方向走方向事实表快照（base 解码已记录/写回路径已同步）
                        let orient = orient;
                        match decode_image_orient(
                            &source, FULL_MAX_DIM, true, factor, &name, &sharpen, 0, Some(orient),
                        ) {
                            Ok((Decoded::Static(img), _, _)) => Ok(img),
                            // 单帧动图取唯一帧当静态原图
                            Ok((Decoded::Anim(mut a), _, _)) if a.len() == 1 => {
                                Ok(a.pop().expect("len==1").0)
                            }
                            Ok(_) | Err(_) => Err(()),
                        }
                    }
                }));
                let msg = match r {
                    Ok(Ok(img)) => ImgMsg::FullReady { key, img, name, epoch, io: did_io },
                    Ok(Err(())) | Err(_) => ImgMsg::FullFailed { key, epoch, io: did_io },
                };
                let _ = tx.send(msg);
                ctx.request_repaint();
            });
        if r.is_err() {
            self.full_pending = None;
            if did_io {
                self.sched.complete(IoClass::Decode);
            }
        }
    }

    /// 每帧排空回传通道：上传纹理进 LRU，收集事件
    pub fn poll(&mut self) -> Vec<ImgEvent> {
        // 看门狗：pending 超时（线程卡死/消息丢失的兜底）——剔除允许
        // 下次 play/预取重新发起，否则该图永久占位
        let now = Instant::now();
        let expired: Vec<(ImgKey, IoClass)> = self
            .pending
            .iter()
            .filter(|(_, (t, ..))| now.duration_since(*t) > PENDING_TIMEOUT)
            .map(|(k, (_, c, _))| (*k, *c))
            .collect();
        for (k, c) in expired {
            self.pending.remove(&k);
            // 回账：卡死线程占住的发牌名额立即释放（其 worker 若还活着，
            // 迟到消息的 complete 会双记一次——饱和下溢 fail-open，
            // 见 iosched.rs 记账口径）
            self.sched.complete(c);
            tracing::warn!("解码超时（>{PENDING_TIMEOUT:?}）剔除，可重试: {k:?}");
        }
        // 原图看门狗：超时判死并标记放弃（区别于基础解码的可重试——重触发
        // 条件 zoom≥0.95 在显示期间恒成立，可重试会无限重解卡死图）
        if let Some((k, t0, io)) = self.full_pending {
            if now.duration_since(t0) > PENDING_TIMEOUT {
                tracing::warn!("原图解码超时（>{PENDING_TIMEOUT:?}）放弃: {k:?}");
                self.full_pending = None;
                self.full_failed = Some(k);
                // 回账（口径同上：迟到消息双记 fail-open；纯 CPU full 未占槽不回账）
                if io {
                    self.sched.complete(IoClass::Decode);
                }
            }
        }
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                ImgMsg::Ready {
                    key,
                    decoded,
                    hist,
                    name,
                    native,
                    orient,
                    class,
                    gen,
                } => {
                    // 代次验证（先验再 remove：pending 里的条目可能是按新
                    // 代次重发的请求，误 remove 会吃掉新请求的占位，新 worker
                    // 结果反而无处落地）：代次不符 = 本 worker 已被
                    // invalidate/看门狗剔除——迟到产物（旧解码/增强配置的
                    // 纹理）丢弃，但回账照常（fail-open：pending 可能被提前
                    // 剔除双记一次——饱和下溢不卡死）
                    let gen_ok = self.pending.get(&key).is_some_and(|(_, _, g)| *g == gen);
                    self.sched.complete(class);
                    if !gen_ok {
                        tracing::debug!("迟到 Ready 丢弃（代次不符）[{name}]");
                        continue;
                    }
                    self.pending.remove(&key);
                    // 方向事实表：R 键旋转基准（无覆盖时 = 文件 tag 值）
                    self.orientations.insert(key, orient);
                    // 原图入 LRU（容量随当前栏数自适应 = 纹理缓存保护集同
                    // 语义"栏数 + 2"，单图 3、对比 n 栏 n+2；封顶逐出最旧；
                    // full 就绪/失败不再清空——像素探针在 full 档显示下仍需
                    // 原图取样）
                    if let Some(n) = native {
                        // orig-lru 预算准入：超单张准入（预算/2）不驻留（探针/
                        // full 复用/对比度重建对该图退回旧行为）。张数 cap +
                        // 字节预算双护栏逐出最旧；当前显示与对比保护集跳过
                        //（全保护时暂时超支，随显示切换自然可逐出）
                        let bytes = n.as_bytes().len();
                        if bytes <= self.orig_budget / 2 {
                            self.natives.push_front((key, n));
                            self.natives_bytes += bytes;
                            let cap = self
                                .shown_multi
                                .len()
                                .max(if self.shown.is_some() { 1 } else { 0 })
                                + 2;
                            while self.natives.len() > cap
                                || self.natives_bytes > self.orig_budget
                            {
                                let Some(pos) = self
                                    .natives
                                    .iter()
                                    .rposition(|(k, _)| !self.is_protected(k))
                                else {
                                    break;
                                };
                                if let Some((_, old)) = self.natives.remove(pos) {
                                    self.natives_bytes -= old.as_bytes().len();
                                }
                            }
                        }
                    }
                    // 计时：与解码日志对照，量化"解码 vs 纹理上传"占比
                    let t = std::time::Instant::now();
                    // per-file 对比度：poll 上传期应用 LUT——解码发起在先、调
                    // 对比度在后也恒拿到最新值（worker 不读 contrast_map，
                    // 无竞态）；n≠0 时直方图改在 LUT 后像素上重算（"显示帧
                    // 与直方图一致"语义），worker 按原值算的直方图丢弃；
                    // n==0 原样直传零开销
                    let lut = {
                        let m = self.contrast_map.borrow();
                        m.get(&key).and_then(|&n| contrast_lut(n))
                    };
                    let (frames, delays, hist) = match decoded {
                        Decoded::Static(mut image) => {
                            if let Some(l) = &lut {
                                apply_contrast(&mut image, l);
                            }
                            let hist = match &lut {
                                Some(_) => Some(Histogram::compute(&image)),
                                None => hist,
                            };
                            let tex = self.tex(&key, 0, image);
                            (vec![tex], vec![Duration::ZERO], hist)
                        }
                        Decoded::Anim(anim) => {
                            let delays = anim.iter().map(|(_, d)| *d).collect::<Vec<_>>();
                            let mut first: Option<egui::ColorImage> = None;
                            let frames = anim
                                .into_iter()
                                .enumerate()
                                .map(|(i, (mut img, _))| {
                                    if let Some(l) = &lut {
                                        apply_contrast(&mut img, l);
                                    }
                                    if i == 0 {
                                        first = Some(img.clone());
                                    }
                                    self.tex(&key, i, img)
                                })
                                .collect::<Vec<_>>();
                            let hist = match &lut {
                                Some(_) => first.as_ref().map(Histogram::compute),
                                None => hist,
                            };
                            (frames, delays, hist)
                        }
                    };
                    let n = frames.len();
                    self.cache_push(
                        key,
                        Cached {
                            frames,
                            delays,
                            cur: 0,
                            next_at: None,
                            hist,
                        },
                    );
                    tracing::debug!(
                        "纹理上传[{name}] lru {} 帧: {}ms",
                        n,
                        t.elapsed().as_millis()
                    );
                    out.push(ImgEvent::Ready(key));
                }
                ImgMsg::Failed { key, error, class, gen } => {
                    // 代次先验再 remove（同 Ready：旧 worker 迟到失败不吃掉
                    // 新代次请求的 pending 占位）；回账照常 fail-open
                    if self.pending.get(&key).is_some_and(|(_, _, g)| *g == gen) {
                        self.pending.remove(&key);
                        out.push(ImgEvent::Failed(key, error));
                    }
                    self.sched.complete(class);
                }
                ImgMsg::FullReady { key, mut img, name, epoch, io } => {
                    // 调度器回账（真源读的 full 占槽；fail-open：无论 pending/
                    // 代次匹配与否都回，双记饱和下溢无害）
                    if io {
                        self.sched.complete(IoClass::Decode);
                    }
                    // 只认当前 pending 的 key + 当前代次：槽已换条目/被
                    // invalidate 的旧 worker 迟到消息丢弃（其产物是旧配置
                    // 参数，不能顶掉 invalidate 后按新配置发起的请求）。
                    // 代次先验再 take——误 take 会吃掉新请求的 pending，
                    // 新 worker 的结果反而无处落地
                    if self.full_pending.as_ref().is_some_and(|(k, _, _)| *k == key)
                        && epoch == self.full_epoch
                    {
                        self.full_pending = None;
                        let t = std::time::Instant::now();
                        // full 纹理同步 per-file 对比度（与 base 纹理同标尺，深
                        // 放大与 base 档观感连续）
                        let g = self.contrast_map.borrow().get(&key).copied().unwrap_or(0);
                        if let Some(l) = contrast_lut(g) {
                            apply_contrast(&mut img, &l);
                        }
                        let tex = self.ctx.load_texture(
                            format!("img-full-{}-{}", key.0, key.1),
                            img,
                            TEX_OPTS_FULL,
                        );
                        tracing::debug!("纹理上传[{name}] full: {}ms", t.elapsed().as_millis());
                        self.full = Some((key, tex));
                        // 原图保留在 natives LRU（像素探针在 full 档显示下
                        // 仍需原图取样，不再随 full 就绪释放）
                    }
                }
                ImgMsg::FullFailed { key, epoch, io } => {
                    // 调度器回账（同 FullReady：真源读占槽，纯 CPU 不占）
                    if io {
                        self.sched.complete(IoClass::Decode);
                    }
                    if self.full_pending.as_ref().is_some_and(|(k, _, _)| *k == key)
                        && epoch == self.full_epoch
                    {
                        self.full_pending = None;
                        tracing::warn!("原图解码失败，保持降采样纹理: {key:?}");
                        self.full_failed = Some(key);
                    }
                }
            }
        }
        // deferred 队列放行（两遍扫描，优先级涌现）：
        // 第一遍只放 Decode/Full（用户可见请求，同类严格 FIFO）——admission
        // 是全局槽位算术，被阻塞的同类必同样阻塞后面同类，跳过不改变顺序；
        // 第二遍只从队首放 Prefetch（低优先级垫底）——否则解码完成瞬间
        // 队首的陈旧预取会先于用户点击的图放行（NAS 上秒级可感），且让路
        // 期被阻塞的预取不能 HOL 卡住后面的用户请求。放行走与 request 同
        // 一 spawn 入口（发牌记账 + pending 占位 + spawn）
        // 第一遍：Decode / Full
        let mut i = 0;
        while i < self.deferred.len() {
            let d = self.deferred[i].clone();
            match &d.kind {
                // 预取留给第二遍（低优先级不参与本轮竞争）：空分支落到底部
                // 统一 i += 1——此处再自增会双重递增，跳过紧随其后的
                // Decode/Full 条目（优先级反转复发，回归测试抓过）
                DeferredKind::Base { class: IoClass::Prefetch, .. } => {}
                DeferredKind::Base { class, .. } => {
                    // 幂等去重（任意位置可弃）
                    if self.pos(d.key).is_some() || self.pending.contains_key(&d.key) {
                        self.deferred.remove(i);
                        continue;
                    }
                    if self.sched.admission(*class) {
                        self.deferred.remove(i);
                        tracing::info!(
                            "IO 放行 {}（排队 {}ms）",
                            d.name,
                            d.at.elapsed().as_millis()
                        );
                        let DeferredKind::Base { max_dim, class } = d.kind else {
                            unreachable!("Base 分支")
                        };
                        self.spawn_base(d.key, d.source, max_dim, &d.name, class);
                        continue;
                    }
                }
                DeferredKind::Full { sharpen } => {
                    // 已满足/已放弃的条目丢弃；full 单槽在途保持队内等待
                    // （触发方每帧重触发不会重复入队——dispatch_full 幂等）
                    if self.has_full(d.key) || self.is_full_failed(d.key) {
                        self.deferred.remove(i);
                        continue;
                    }
                    if self.full_pending.is_none()
                        && self.dispatch_full(d.key, d.source.clone(), &d.name, *sharpen)
                    {
                        self.deferred.remove(i);
                        tracing::info!(
                            "IO 放行 {}（排队 {}ms）",
                            d.name,
                            d.at.elapsed().as_millis()
                        );
                        continue;
                    }
                }
            }
            i += 1;
        }
        // 第二遍：Prefetch（严格队首 FIFO，被阻塞即停——同类保序）
        while let Some(d) = self.deferred.front().cloned() {
            let DeferredKind::Base { class: IoClass::Prefetch, max_dim, .. } = d.kind else {
                break;
            };
            if self.pos(d.key).is_some() || self.pending.contains_key(&d.key) {
                self.deferred.pop_front();
                continue;
            }
            if !self.sched.admission(IoClass::Prefetch) {
                break;
            }
            self.deferred.pop_front();
            tracing::info!("IO 放行 {}（排队 {}ms）", d.name, d.at.elapsed().as_millis());
            self.spawn_base(d.key, d.source, max_dim, &d.name, IoClass::Prefetch);
        }
        out
    }

    /// 像素探针取样（alt+点击）：从原图 LRU 按归一化图像坐标 (0..1) 取
    /// 最近像素的 RGB。显示纹理是重采样/插值结果，取样值必须来自原图；
    /// 返回 (原图像素坐标, RGB)。原图未保留（动图/超大图/已逐出）= None
    pub fn original_pixel(&self, key: ImgKey, uv: egui::Vec2) -> Option<((u32, u32), [u8; 3])> {
        let (_, img) = self.natives.iter().find(|(k, _)| *k == key)?;
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }
        let x = ((uv.x * w as f32) as i32).clamp(0, w as i32 - 1) as u32;
        let y = ((uv.y * h as f32) as i32).clamp(0, h as i32 - 1) as u32;
        let p = img.get_pixel(x, y);
        Some(((x, y), [p[0], p[1], p[2]]))
    }

    /// 原图尺寸（像素探针跨栏同步判分辨率相等用；未保留 = None）
    pub fn native_size(&self, key: ImgKey) -> Option<(u32, u32)> {
        self.natives
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, img)| (img.width(), img.height()))
    }

    /// 当前应显示的帧（动图按 next_at 推进切帧）。返回 `(纹理, 距下次切帧)`，
    /// 静态图/未到点 = None 时长。UI 每帧对当前 key 调用并
    /// `request_repaint_after(delay)` 驱动动画；命中即提至 LRU 队首
    pub fn frame(&mut self, key: ImgKey) -> Option<(egui::TextureHandle, Option<Duration>)> {
        self.frame_ex(key, true)
    }

    /// 直方图（静态图全图 / 动图首帧），随纹理进 LRU。未解码/已逐出 =
    /// None；UI 覆盖层据此显示"解码中/不可用"。不触碰 LRU 时序（不像
    /// frame() 提队首——只读旁路，不干扰翻页保护语义）
    pub fn histogram(&self, key: ImgKey) -> Option<&Histogram> {
        self.pos(key)
            .and_then(|p| self.cache.get(p))
            .and_then(|(_, c)| c.hist.as_ref())
    }

    /// orig-lru 逐出保护：单图当前显示或对比显示集合内的条目不可逐出
    fn is_protected(&self, k: &ImgKey) -> bool {
        self.shown.as_ref() == Some(k) || self.shown_multi.contains(k)
    }

    /// per-file 对比度 W/S 重放（App 写 contrast_map 后调用）。natives 命中 →
    /// finish_static 精确复现 base 管线（sharpen OFF，与首次解码同代码路径）
    /// → LUT → 上传 + 直方图重算，毫秒级；未命中（动图 / 超预算巨图 /
    /// native 已逐出）→ 清该条纹理缓存返回 false，调用方重发 request——
    /// 解码结果在 poll 上传期按最新对比度落 LUT，连按自洽无需防抖。
    /// n==0 同样走重建（从带旧对比度的显示纹理回到原值）。full 槽同步
    /// 失效——缩放中的 full 由现有 per-frame ensure 按需重触发
    ///（natives 命中免二次熵解码）
    pub fn refresh_contrast(&mut self, key: ImgKey, max_dim: u32) -> bool {
        if self.full.as_ref().is_some_and(|(k, _)| *k == key) {
            self.full = None;
        }
        if self.full_failed.as_ref() == Some(&key) {
            self.full_failed = None;
        }
        let g = self.contrast_map.borrow().get(&key).copied().unwrap_or(0);
        let Some(native) = self
            .natives
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, n)| n.clone())
        else {
            if let Some(pos) = self.pos(key) {
                self.cache.remove(pos);
            }
            return false;
        };
        let factor = (self.ctx.pixels_per_point() as f64).max(1.0);
        let rebuilt = finish_static(native, max_dim, false, factor, &SharpenCfg::OFF, 0)
            .ok()
            .and_then(|(d, _)| match d {
                Decoded::Static(out) => Some(out),
                _ => None,
            });
        let Some(mut img) = rebuilt else {
            if let Some(pos) = self.pos(key) {
                self.cache.remove(pos);
            }
            return false;
        };
        if let Some(l) = contrast_lut(g) {
            apply_contrast(&mut img, &l);
        }
        let hist = Histogram::compute(&img);
        let tex = self.tex(&key, 0, img);
        let cached = Cached {
            frames: vec![tex],
            delays: vec![Duration::ZERO],
            cur: 0,
            next_at: None,
            hist: Some(hist),
        };
        // 原条目在位则原位替换（保留 LRU 位置）；不在位（refresh 先于
        // 首次上传到达等边缘时序）按新条目推入
        if self.pos(key).is_some() {
            self.cache.remove(self.pos(key).expect("pos 有效"));
        }
        self.cache_push(key, cached);
        true
    }

    /// 对比模式变体：非主栏调用传 `drop_stale_full=false`——多栏每帧轮流
    /// frame() 不同 key，若人人都弃槽会把主栏刚升级的 full 纹理立即抖掉；
    /// 只有主栏（真正拥有 full 槽的栏）走 true，主栏切换时自然淘汰旧槽
    pub fn frame_ex(
        &mut self,
        key: ImgKey,
        drop_stale_full: bool,
    ) -> Option<(egui::TextureHandle, Option<Duration>)> {
        let pos = self.pos(key)?;
        // 原图单槽生命周期跟随显示条目：切条目即弃（旧原图显存大不留驻）；
        // 放弃标记不清——解码失败是确定性的，重试只会再失败一遍
        if drop_stale_full && self.full.as_ref().is_some_and(|(k, _)| *k != key) {
            self.full = None;
        }
        // 标记显示中：cache_push 逐出时跳过（预取洪水不再挤掉正在显示的图）
        self.shown = Some(key);
        let (_, mut c) = self.cache.remove(pos).expect("pos 有效");
        if c.frames.len() > 1 {
            let now = Instant::now();
            match c.next_at {
                // 首次显示：起播时钟（当前帧延迟后切下一帧）
                None => c.next_at = Some(now + c.delays[c.cur]),
                Some(next) if now >= next => {
                    c.cur = (c.cur + 1) % c.frames.len();
                    c.next_at = Some(now + c.delays[c.cur]);
                }
                _ => {}
            }
        }
        let rest = c
            .next_at
            .filter(|_| c.frames.len() > 1)
            .map(|t| t.saturating_duration_since(Instant::now()));
        let frame = c.frames[c.cur].clone();
        self.cache.push_front((key, c));
        // 原图同步提队首：native 生命周期跟随纹理 LRU 时序（"纹理在 ⇔ 原图
        // 在"不变量）。否则来回浏览后两队列顺序分叉——纹理因最近查看留在
        // tex-lru，native 却因解码较早被挤出且切回时 has() 命中纹理不重解码，
        // native 永不回归 → 像素探针永久"原图未保留"（shoot dump 实测踩中）
        if let Some(i) = self.natives.iter().position(|(k, _)| *k == key) {
            let n = self.natives.remove(i).expect("i 有效");
            self.natives.push_front(n);
        }
        Some((frame, rest))
    }

    /// 上传一帧纹理（命名带帧号，GPU profiler 可读）
    fn tex(&self, key: &ImgKey, i: usize, image: egui::ColorImage) -> egui::TextureHandle {
        self.ctx
            .load_texture(format!("img-{}-{}-f{i}", key.0, key.1), image, TEX_OPTS)
    }

    /// 缓存命中判定（不打乱 LRU 顺序；`cached` 会把命中项移到队首）
    pub fn has(&self, key: ImgKey) -> bool {
        self.pos(key).is_some()
    }

    /// 解码中（含等待 poll）判定
    pub fn is_pending(&self, key: ImgKey) -> bool {
        self.pending.contains_key(&key)
    }

    /// 清空 deferred 队列（换压缩包/进出对比时调用）：队列条目持有旧源
    /// Arc<VFS>，条目键对新会话已无意义；队首若滞留旧请求还会挡住新请求
    /// 放行（FIFO 不插队）
    pub fn clear_deferred(&mut self) {
        self.deferred.clear();
    }

    /// deferred 队列长度（评级派发优先级闸门用：非空 = 有解码/预取在排队）
    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    /// 指定条目是否在 deferred 队列中（调度观测：排队汇总/测试断言用）
    pub fn is_deferred(&self, key: ImgKey) -> bool {
        self.deferred.iter().any(|d| d.key == key)
    }

    /// 静态图判定（动图不做原图升级：全帧原图显存不可控）
    pub fn is_static(&self, key: ImgKey) -> bool {
        self.cache
            .iter()
            .any(|(k, c)| *k == key && c.frames.len() == 1)
    }

    /// 原图纹理是否就绪
    pub fn has_full(&self, key: ImgKey) -> bool {
        self.full.as_ref().is_some_and(|(k, _)| *k == key)
    }

    /// 原图解码中判定
    pub fn is_full_pending(&self, key: ImgKey) -> bool {
        self.full_pending.is_some_and(|(k, _, _)| k == key)
    }

    /// 原图已放弃判定（失败/超时/动图）：不再重试
    pub fn is_full_failed(&self, key: ImgKey) -> bool {
        self.full_failed == Some(key)
    }

    /// 取原图纹理（匹配单槽才有效；未就绪 = None）
    pub fn full_tex(&self, key: ImgKey) -> Option<egui::TextureHandle> {
        self.full
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, t)| t.clone())
    }

    /// 屏幕密度（发起解码与 UI"100%"落点共用同一取值来源）
    pub fn ppp(&self) -> f32 {
        self.ctx.pixels_per_point()
    }

    fn pos(&self, key: ImgKey) -> Option<usize> {
        self.cache.iter().position(|(k, _)| *k == key)
    }

    /// 入缓存并逐出最久未用（超容量时）；逐出跳过 key（刚推入）、shown
    /// （正在显示）与 shown_multi（对比模式各栏）。全都不堪逐时允许临时
    /// 超容（break），下次推入再收缩
    fn cache_push(&mut self, key: ImgKey, c: Cached) {
        self.cache.push_front((key, c));
        let shown = self.shown;
        while self.cache.len() > self.cap {
            let Some(victim) = self.cache.iter().rposition(|(k, _)| {
                *k != key && Some(*k) != shown && !self.shown_multi.contains(k)
            }) else {
                break;
            };
            self.cache.remove(victim);
        }
    }
}

/// per-file 显示对比度步进标尺：n = 按键次数（正 = 增强），每步对比度斜率
/// ×1.25（降低 ×0.8 = 精确逆操作）。图片/raw 软解 LUT 路径的步数上限：
/// 斜率 1.25^6 ≈ 3.8 时画面已接近黑白两色，再大无意义
pub(crate) const CONTRAST_STEP_MAX: i32 = 6;
/// mpv 路径步数上限：contrast 属性域 ±100，一步斜率 ×1.25 ≈ contrast ±25，
/// ±4 步即触属性边界——钳住防"OSD 倍率在涨画面却不再变化"
pub(crate) const CONTRAST_STEP_MAX_MPV: i32 = 4;

/// 步数 → 对比度斜率倍率（s = 1.25^n；OSD 与 LUT/mpv 换算共用）
pub(crate) fn contrast_factor(n: i32) -> f64 {
    1.25f64.powf(n as f64)
}

/// OSD 文案：步数 → 倍率显示（0 = 复位）
pub(crate) fn contrast_label(n: i32) -> String {
    if n == 0 {
        "对比度 ×1.00（复位）".into()
    } else {
        format!("对比度 ×{:.2}", contrast_factor(n))
    }
}

/// per-file 显示对比度 LUT（W/S 快捷键）：n = 步数（0 = 不调）。绕固定锚点
/// 127.5 的线性映射 out = (v−127.5)×s + 127.5（s = 1.25^n），与 mpv contrast
/// 属性同一条曲线（斜率 = 1 + c/100，c = 25n）：锚点不动——黑还是黑、白还是
/// 白，无 gamma 提亮"黑位发灰"问题；s>1 增强（极部饱和裁剪是对比度预期
/// 行为）、s<1 降低（向中灰收敛）。None = n==0（恒等——调用方跳过整帧
/// 遍历，未调零成本）。探针读 natives 原图，不经 LUT（忠实存储原值语义，
/// 与显示增益分离）
pub(crate) fn contrast_lut(n: i32) -> Option<[u8; 256]> {
    if n == 0 {
        return None;
    }
    let s = 1.25f64.powf(n as f64);
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = ((i as f64 - 127.5) * s + 127.5).round().clamp(0.0, 255.0) as u8;
    }
    Some(lut)
}

/// LUT 逐像素应用到 ColorImage（RGB 查表，alpha 不动）。O(像素) 单次遍历，
/// 4K 图 ~10ms——W/S 按键重放在 UI 线程可接受
pub(crate) fn apply_contrast(img: &mut egui::ColorImage, lut: &[u8; 256]) {
    for px in &mut img.pixels {
        // ColorImage 像素为非预乘 RGBA（from_rgba_unmultiplied 构造），
        // 重建时保持 alpha 通道语义一致
        *px = egui::Color32::from_rgba_unmultiplied(
            lut[px.r() as usize],
            lut[px.g() as usize],
            lut[px.b() as usize],
            px.a(),
        );
    }
}

/// 后台线程主体：读源 → 解码（gif/webp 逐帧）→ 尺寸适配 → RGBA。
/// full=false 基础档（factor=ppp）/ true 高清档（factor=ppp×3），
/// 两档策略见 resample；factor 由 ImageService 按屏幕 DPI 内部取值；
/// sharpen = 高清档放大前观感增强配置（config image.image_full_sharpen*，
/// 基础档传 [`SharpenCfg::OFF`]）
/// 返回 (结果, 原图保留)：原图仅基础档静态且满足 orig-lru 准入时带回
///（小图 native×factor ≤ max_dim 同预放大判据，或字节数 ≤ 单张准入
/// native_limit_bytes = orig 预算/2），供 full 升级复用/像素探针/per-file
/// 对比度重建三个消费者使用
/// 旧签名包装（现仅测试调用；生产链路走 decode_image_orient）。
#[cfg(test)]
fn decode_image(
    source: &ImageSource,
    max_dim: u32,
    full: bool,
    factor: f64,
    name: &str,
    sharpen: &SharpenCfg,
    native_limit_bytes: u64,
) -> Result<(Decoded, Option<image::DynamicImage>), String> {
    // 无方向覆盖的默认入口（单测与 full 档旧调用形态）
    decode_image_orient(source, max_dim, full, factor, name, sharpen, native_limit_bytes, None)
        .map(|(d, n, _)| (d, n))
}

/// 方向参数化版本：orient_override = Some(u8 1-8) 时覆盖文件 EXIF 方向
///（R 键旋转、不可写格式的会话内记忆路径），None = 读文件元数据。
/// 返回值第三项 = 实际应用的 EXIF 方向值（静态图；动图恒 1），
/// 供 ImageService 记录方向事实（R 键在此值上轮转）。
fn decode_image_orient(
    source: &ImageSource,
    max_dim: u32,
    full: bool,
    factor: f64,
    name: &str,
    sharpen: &SharpenCfg,
    native_limit_bytes: u64,
    orient_override: Option<u8>,
) -> Result<(Decoded, Option<image::DynamicImage>, u8), String> {
    match source {
        ImageSource::File(path) => {
            // 相机 RAW 拦截：with_guessed_format 对 ARW/NEF 探不出格式
            //（返回 None），须按扩展名先行拦截（方案 §4.3）。失败 fall
            // through 原路径——改名伪 raw 由 ImageReader 报错 → mpv 回退
            if is_raw_photo_ext(name) {
                if let Ok((img, o)) = decode_raw_preview_path(path, orient_override) {
                    tracing::debug!("RAW 预览 [{name}] {}x{} orient={o}", img.width(), img.height());
                    return finish_static(img, max_dim, full, factor, sharpen, native_limit_bytes)
                        .map(|(d, n)| (d, n, o));
                }
            }
            // EXIF 方向（R 旋转覆盖优先；kamadak-exif 读文件头，畸形文件
            // 静默回 1——方向缺失不影响解码主链路）
            let orient = orient_override.unwrap_or_else(|| {
                crate::meta::read_meta_path(path).map(|m| m.orientation).unwrap_or(1)
            });
            let reader = image::ImageReader::open(path)
                .map_err(|e| format!("打开图片失败: {e}"))?
                .with_guessed_format()
                .map_err(|e| format!("识别图片格式失败: {e}"))?;
            decode_any(reader, max_dim, full, factor, name, sharpen, native_limit_bytes, orient)
        }
        ImageSource::Stream(vfs, entry) => {
            let mut vf = vfs
                .open_entry(entry)
                .map_err(|e| format!("打开压缩包条目失败: {e}"))?;
            // 条目大小已知：按声明容量预分配（防恶意声明封顶 1 GiB）
            let mut buf = Vec::with_capacity(entry.size.clamp(0, 1024 * 1024 * 1024) as usize);
            vf.read_to_end(&mut buf)
                .map_err(|e| format!("读取压缩包条目失败: {e}"))?;
            // 同 File 分支：RAW 拦截（buf 已全量读入，from_bytes 直接解析）
            if is_raw_photo_ext(name) {
                if let Ok((img, o)) = decode_raw_preview(&buf, orient_override) {
                    tracing::debug!("RAW 预览 [{name}] {}x{} orient={o}", img.width(), img.height());
                    return finish_static(img, max_dim, full, factor, sharpen, native_limit_bytes)
                        .map(|(d, n)| (d, n, o));
                }
            }
            // EXIF 方向：buf 已在内存，Cursor 头部解析零额外 IO
            let orient = orient_override.unwrap_or_else(|| {
                crate::meta::read_meta_bytes(&buf).map(|m| m.orientation).unwrap_or(1)
            });
            let reader = image::ImageReader::new(std::io::Cursor::new(buf))
                .with_guessed_format()
                .map_err(|e| format!("识别图片格式失败: {e}"))?;
            decode_any(reader, max_dim, full, factor, name, sharpen, native_limit_bytes, orient)
        }
    }
}

/// 按格式分流：gif 与动画 webp 走逐帧解码（单帧归静态）；静态 webp 与其余
/// 格式走单帧 decode。webp 经 `has_animation()`（VP8X 头 ANIM 位）先验分流：
/// 静态文件 `num_frames()==0`、`into_frames` 解出空集（LOG2 踩坑），不可盲目逐帧
fn decode_any<R: BufRead + Seek>(
    reader: image::ImageReader<R>,
    max_dim: u32,
    full: bool,
    factor: f64,
    name: &str,
    sharpen: &SharpenCfg,
    native_limit_bytes: u64,
    orient: u8,
) -> Result<(Decoded, Option<image::DynamicImage>, u8), String> {
    let fmt = reader.format();
    // 日志用短名（压缩包条目路径不含目录）
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    if fmt == Some(image::ImageFormat::Gif) {
        let t = std::time::Instant::now();
        let dec = image::codecs::gif::GifDecoder::new(reader.into_inner())
            .map_err(|e| format!("打开 GIF 失败: {e}"))?;
        let frames = collect_frames(dec.into_frames(), max_dim)?;
        tracing::debug!(
            "解码 gif [{base}] {} 帧: {}ms",
            frames.len(),
            t.elapsed().as_millis()
        );
        // 动图（含单帧）不保留原图：不做 full 升级，缓存白驻。
        // 动图无方向语义（帧序列已含变换），EXIF 方向不适用
        return Ok(if frames.len() == 1 {
            let (img, _) = frames.into_iter().next().expect("len==1");
            (Decoded::Static(img), None, 1)
        } else {
            (Decoded::Anim(frames), None, 1)
        });
    }
    let t = std::time::Instant::now();
    // webp 分流：动画（VP8X ANIM 位置位）复用 gif 全套逐帧机制——帧迭代
    // 产画布尺寸合成帧 + 毫秒延迟，与 gif 帧完全同构；静态走 ImageDecoder
    // 直解（read_image 自带 seek 回图像数据块），接下方共享尾部
    let img = if fmt == Some(image::ImageFormat::WebP) {
        let dec = image::codecs::webp::WebPDecoder::new(reader.into_inner())
            .map_err(|e| format!("打开 WebP 失败: {e}"))?;
        if dec.has_animation() {
            let t = std::time::Instant::now();
            let frames = collect_frames(dec.into_frames(), max_dim)?;
            tracing::debug!(
                "解码 webp 动画 [{base}] {} 帧: {}ms",
                frames.len(),
                t.elapsed().as_millis()
            );
            // 同 gif：动图（含单帧）不保留原图；方向不适用
            return Ok(if frames.len() == 1 {
                let (img, _) = frames.into_iter().next().expect("len==1");
                (Decoded::Static(img), None, 1)
            } else {
                (Decoded::Anim(frames), None, 1)
            });
        }
        image::DynamicImage::from_decoder(dec).map_err(|e| format!("解码图片失败: {e}"))?
    } else {
        reader.decode().map_err(|e| format!("解码图片失败: {e}"))?
    };
    // EXIF 方向矫正（≤4096 像素级重排，被解码耗时掩盖）：手机横拍照片
    // 依赖此步正立显示；R 键旋转覆盖与文件 tag 同走一个 T(o) 变换
    //（PLAN_image_meta.md §5.A）
    let img = crate::meta::apply_orientation(img, orient);
    // 两档尺寸适配（策略见 resample）：基础档 LRU 存 native×ppp（翻页
    // 即 100% 密度直出）；高清档 full 槽存 native×ppp×3（深放大用，
    // clamp 12288 + 像素预算）。factor 由 ImageService 按屏幕 DPI 定。
    // 共享尾部抽为 finish_static，与 RAW 预览拦截分支共用（方案 §4.3）
    let (w, h) = (img.width(), img.height());
    let (decoded, native) = finish_static(img, max_dim, full, factor, sharpen, native_limit_bytes)?;
    // 尺寸命名日志：native_size 原图 / lru_size 基础纹理 / full_size
    // full 槽纹理（箭头一致 = 未触发重采样，尺寸原样直进）
    let Decoded::Static(out) = &decoded else {
        unreachable!("finish_static 恒返回 Static")
    };
    tracing::debug!(
        "解码 {fmt:?} [{base}] orient={orient} native_size={w}x{h} {}_size={}x{}{}: {}ms",
        if full { "full" } else { "lru" },
        out.size[0],
        out.size[1],
        if full { sharpen.tag() } else { "" },
        t.elapsed().as_millis()
    );
    Ok((decoded, native, orient))
}

/// 静态图共享尾部：可选降噪+锐化（config image.image_full_sharpen*，仅高清
/// 档，**放大前**在原图分辨率执行——把边缘坡度先变陡，Lanczos 插值保持
/// 陡峭过渡，成本从放大后千万像素级降到原图级，见 [`sharpen_full`]）→
/// 两档尺寸适配（见 [`resample`]）→ RGBA，并按 orig-lru 准入判据决定
/// 是否保留原图（三个消费者：full 升级复用 / 像素探针 / per-file 对比度
/// native 重建）。[`decode_any`] 与 RAW 预览拦截分支共用
fn finish_static(
    img: image::DynamicImage,
    max_dim: u32,
    full: bool,
    factor: f64,
    sharpen: &SharpenCfg,
    native_limit_bytes: u64,
) -> Result<(Decoded, Option<image::DynamicImage>), String> {
    // shoot_v 边缘掩码暂存（sharpen_full 产出 → resample 后的 antiring_step
    // 消费——两步夹着 resample，用局部 Option 中转）
    let mut shoot_mask_holder = None;
    // 观感增强入口判定：降噪独立于锐化开关（sharpen.on || denoise）——
    // 全关时零开销直通
    let img = if full && (sharpen.on || sharpen.denoise) {
        let (enhanced, shoot_mask) = sharpen_full(img, sharpen);
        shoot_mask_holder = shoot_mask;
        enhanced
    } else {
        img
    };
    // 原图保留（仅基础档）：full=true 时原图即输入，无需缓存。
    // 准入判据（任一满足）：① 小图（native×factor ≤ max_dim，同预放大
    // 判据——×ppp 后仍小，驻留便宜）；② 字节数 ≤ 单张准入（orig 预算/2，
    // budget 制：大图也驻留，服务探针/full 复用/对比度重建）
    let native = if full {
        None
    } else {
        let longest = img.width().max(img.height());
        let fits_tex = (longest as f64 * factor).round() <= max_dim as f64;
        let fits_budget = native_limit_bytes > 0 && img.as_bytes().len() as u64 <= native_limit_bytes;
        (fits_tex || fits_budget).then(|| img.clone())
    };
    // 防振铃源 rgba 提取（掩码存在的充要前提：antiring 开 + 锐化开 + 白边
    // 抑制开——三者缺一掩码为 None，antiring_step 直通，src 提取也跳过）
    let src_rgba = if full && sharpen.antiring && sharpen.on && sharpen.white_edge.is_some() {
        Some(img.to_rgba8())
    } else {
        None
    };
    let out = resample(img, max_dim, full, factor);
    let out = antiring_step(src_rgba, shoot_mask_holder.take(), out);
    Ok((Decoded::Static(color_image(out)), native))
}

/// full 槽构建（原图缓存命中路径，免读源）：可选降噪+锐化（原图分辨率）
/// → ×factor Lanczos 上采样（clamp + 像素预算）→ 防振铃 → RGBA。
/// 与解码路径的 finish_static 同语义（含耗时打印，缓存命中路径同样可见）
fn build_full(img: image::DynamicImage, factor: f64, sharpen: &SharpenCfg) -> egui::ColorImage {
    let (img, shoot_mask) = if sharpen.on || sharpen.denoise {
        let (enhanced, mask) = sharpen_full(img, sharpen);
        (enhanced, mask)
    } else {
        (img, None)
    };
    // 防振铃源 rgba 提取（掩码存在前提同 finish_static：antiring + 锐化 +
    // 白边抑制三者齐备，否则 antiring_step 直通且免拷贝）
    let src_rgba = if sharpen.antiring && sharpen.on && sharpen.white_edge.is_some() {
        Some(img.to_rgba8())
    } else {
        None
    };
    let out = resample(img, FULL_MAX_DIM, true, factor);
    let out = antiring_step(src_rgba, shoot_mask, out);
    color_image(out)
}

/// 相机 RAW 家族扩展名（Sony ARW 家族 + Nikon NEF 家族），与 config
/// `image_extensions` 中的 raw 段一致，常量硬编码——拦截发生在
/// wants_texture（is_image_ext）放行后的 decode 层，只认这几家。
/// 用 Path::extension 判定，"arw" 这类无扩展名裸名不算（防误伤）
fn is_raw_photo_ext(name: &str) -> bool {
    const RAW_EXTS: &[&str] = &["arw", "nef", "nrw", "srf", "sr2", "arq"];
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| RAW_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// 相机 RAW 内嵌 JPEG 预览解码（内存源：zip/目录条目已全量读入）。
/// raw-files 只解析文件结构（自带 64KiB 块缓存），读出相机内嵌的最大
/// JPEG 预览再交 image 解码——成本与普通 jpg 同量级，LRU/高清档/
/// 看门狗机制照常生效。识别/解码失败返回 Err，调用方 fall through。
/// 返回 (矫正后位图, 实际应用的 EXIF 方向值)：R 键覆盖优先，否则用
/// raw-files 读出的 Orientation（raw-files 变体与 EXIF 1-8 一一对应）
fn decode_raw_preview(
    bytes: &[u8],
    orient_override: Option<u8>,
) -> Result<(image::DynamicImage, u8), String> {
    let mut f = raw_files::RawFile::from_bytes(bytes).map_err(|e| format!("RAW 识别失败: {e}"))?;
    let pv = f.largest_preview().ok_or("RAW 无内嵌预览")?;
    let jpeg = f
        .read_preview(&pv)
        .map_err(|e| format!("读预览失败: {e}"))?;
    let img = image::load_from_memory(&jpeg).map_err(|e| format!("预览解码失败: {e}"))?;
    let o = orient_override.unwrap_or_else(|| raw_orient_u8(pv.orientation()));
    Ok((crate::meta::apply_orientation(img, o), o))
}

/// 同 [`decode_raw_preview`]，文件源（7z/RAR 落盘缓存）：`RawFile::open`
/// 的块缓存只读文件结构与预览所在块，优于全量读盘
fn decode_raw_preview_path(
    path: &std::path::Path,
    orient_override: Option<u8>,
) -> Result<(image::DynamicImage, u8), String> {
    let mut f = raw_files::RawFile::open(path).map_err(|e| format!("RAW 识别失败: {e}"))?;
    let pv = f.largest_preview().ok_or("RAW 无内嵌预览")?;
    let jpeg = f
        .read_preview(&pv)
        .map_err(|e| format!("读预览失败: {e}"))?;
    let img = image::load_from_memory(&jpeg).map_err(|e| format!("预览解码失败: {e}"))?;
    let o = orient_override.unwrap_or_else(|| raw_orient_u8(pv.orientation()));
    Ok((crate::meta::apply_orientation(img, o), o))
}

/// raw-files `Orientation` → EXIF 1-8 数值（变体名与 image crate 一一对应）
fn raw_orient_u8(o: raw_files::Orientation) -> u8 {
    use raw_files::Orientation as O;
    match o {
        O::NoTransforms => 1,
        O::FlipHorizontal => 2,
        O::Rotate180 => 3,
        O::FlipVertical => 4,
        O::Rotate90FlipH => 5,
        O::Rotate90 => 6,
        O::Rotate270FlipH => 7,
        O::Rotate270 => 8,
    }
}

/// 帧迭代收集：逐帧尺寸适配 + 延迟钳正 + 帧数封顶
fn collect_frames(
    frames: image::Frames,
    max_dim: u32,
) -> Result<Vec<(egui::ColorImage, Duration)>, String> {
    let mut out = Vec::new();
    for fr in frames {
        let fr = fr.map_err(|e| format!("动画帧解码失败: {e}"))?;
        let delay = anim_delay(fr.delay());
        // 动图不预放大（多帧全驻显存，×factor² 爆炸）；超限降采样
        // ——全帧常驻，6000px 级 GIF ×几十帧会爆显存，静图的"原尺寸
        // 直进"不适用；cap 同样受 8192 硬上限约束（防超大配置+病图）
        let img = thumbnail_cap(
            image::DynamicImage::ImageRgba8(fr.into_buffer()),
            max_dim.min(MAX_TEX_DIM),
        );
        out.push((color_image(img), delay));
        if out.len() >= MAX_ANIM_FRAMES {
            tracing::warn!("动图超 {MAX_ANIM_FRAMES} 帧，截断剩余帧");
            break;
        }
    }
    if out.is_empty() {
        return Err("动画无有效帧".into());
    }
    Ok(out)
}

/// 帧显示时长：GIF 惯例——< 20ms（多为浏览器时代的 0 延迟优化写法）钳到
/// 100ms，否则疯闪；分母 0 等病态值按 100ms 兜底。
/// 注意 `numer_denom_ms` 的比值单位本身就是毫秒（image 0.25 语义：
/// from_numer_denom_ms(n, de) = n/de ms，GIF 解码产出 (delay_cs*10, 1)），
/// 不可再 ×1000；换 µs 计算保留亚毫秒精度
fn anim_delay(d: image::Delay) -> Duration {
    let (n, de) = d.numer_denom_ms();
    let d = Duration::from_micros(n as u64 * 1_000 / de.max(1) as u64);
    if d < Duration::from_millis(20) {
        Duration::from_millis(100)
    } else {
        d
    }
}

/// 尺寸适配（静图两档，尺寸命名：native_size 原图 / lru_size 基础纹理
/// / full_size full 槽纹理）：
/// - 病态兜底（基础档共用）：最长边 > MAX_TEX_DIM(8192) 一律降采样，
///   防 30000×30000 级病图撑爆显存（LRU 全驻）
/// - 基础档（full=false，LRU 缓存纹理）：阈值型——native×factor ≤
///   max_dim 才 Lanczos ×factor 预放大（factor=ppp：翻页即 100% 密度
///   直出，不再"先糊后锐"换画）；×factor 超限的原尺寸直进（超大图
///   纹理密度已足，深放大交给 GPU 双线性，免降采样损失与秒级重采样）
/// - 高清档（full=true，full 单槽）：clamp 型——无条件 Lanczos ×factor
///   封顶 FULL_MAX_DIM(12288) + 像素预算 FULL_MAX_PIXELS（factor=ppp×3；
///   native ≤ image_max_dim 资格由调用方保证，从原图（缓存/二次解码）
///   构建，绝不在已放大的纹理上级联再放大）
fn resample(
    img: image::DynamicImage,
    max_dim: u32,
    full: bool,
    factor: f64,
) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return img;
    }
    let longest = w.max(h);
    if !full {
        // 病态兜底（基础档）：LRU 全驻显存，8192²×4B=256MB 封顶
        if longest > MAX_TEX_DIM {
            return thumbnail_cap(img, MAX_TEX_DIM);
        }
        // 阈值型：放大后不超 max_dim 才升（×factor 全图等比，最长边
        // 判定即可），超限原尺寸直进
        if (longest as f64 * factor).round() <= max_dim as f64 {
            upscale(img, factor, max_dim)
        } else {
            img
        }
    } else {
        // clamp 型：无条件 ×factor、封顶 FULL_MAX_DIM + 像素预算
        //（upscale 内二次封顶；病态超大图理论上到不了这，防御性 clamp）
        upscale(img, factor, FULL_MAX_DIM)
    }
}

/// Lanczos3 ×factor 等比上采样（fast_image_resize SIMD 实现）、最长边封顶
/// cap + 总像素预算（按最长边等比缩，触顶/超预算不畸变——逐边独立 clamp
/// 会让非方形图比例失调；尺寸不变时原样返回）
fn upscale(img: image::DynamicImage, factor: f64, cap: u32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    // 目标最长边：×factor 后封顶 cap（≥1 防零图/极端 factor 归零）
    let target = ((longest as f64 * factor).round() as u32).min(cap).max(1);
    let scale = target as f64 / longest as f64;
    let mut nw = ((w as f64 * scale).round() as u32).max(1);
    let mut nh = ((h as f64 * scale).round() as u32).max(1);
    // 像素预算二次封顶：正方形极端图（12288²≈151MP）等比缩回预算内。
    // floor 取整：round 可使整数乘积反超预算（如 11181×8944≈100.003MP），
    // floor(a)·floor(b) ≤ a·b = 预算恒成立
    let pixels = nw as u64 * nh as u64;
    if pixels > FULL_MAX_PIXELS {
        let s = (FULL_MAX_PIXELS as f64 / pixels as f64).sqrt();
        nw = ((nw as f64 * s).floor() as u32).max(1);
        nh = ((nh as f64 * s).floor() as u32).max(1);
    }
    if nw != w || nh != h {
        fir_resize(img, nw, nh)
    } else {
        img
    }
}

/// fast_image_resize Lanczos3（SSE4/AVX2 SIMD）：RGBA8 直入直出。与 image
/// crate 的 FilterType::Lanczos3 同一卷积核，纯提速不动画质（33MP 级
/// ~600ms → ~80ms）。into_rgba8 零拷贝进源缓冲；尺寸/缓冲自洽性由调用方
/// 保证，违例 expect panic 由 worker 线程 catch_unwind 兜成 Failed
fn fir_resize(img: image::DynamicImage, nw: u32, nh: u32) -> image::DynamicImage {
    use fast_image_resize as fir;
    let rgba = img.into_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let src = fir::images::Image::from_vec_u8(w, h, rgba.into_raw(), fir::PixelType::U8x4)
        .expect("源缓冲与尺寸自洽");
    let mut dst = fir::images::Image::new(nw, nh, fir::PixelType::U8x4);
    let mut resizer = fir::Resizer::new();
    let options = fir::ResizeOptions::new()
        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3));
    resizer
        .resize(&src, &mut dst, &options)
        .expect("目标尺寸有效");
    image::DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(nw, nh, dst.into_vec()).expect("目标缓冲与尺寸自洽"),
    )
}

/// 防振铃包装：resample 输出原地 clamp（full 放大档专属）。
/// 两个前置缺失其一即直通（零开销）：①源 rgba 未提取（antiring 关）
/// ②shoot_v 边缘掩码缺失（锐化关/白边抑制关——防振铃依赖锐化链产出的
/// 同一份边缘检测结果做定向，不独立重扫）。独立计时打印——finish_static
/// 解码路径与 build_full 缓存命中路径都走这里，两链路耗时均可见
fn antiring_step(
    src_rgba: Option<image::RgbaImage>,
    shoot_mask: Option<image::GrayImage>,
    out: image::DynamicImage,
) -> image::DynamicImage {
    let (Some(src), Some(mask)) = (src_rgba, shoot_mask) else {
        return out;
    };
    let t = std::time::Instant::now();
    // full 路径 resample 恒产 ImageRgba8（fir_resize），原地改免二次拷贝
    let mut rgba = match out {
        image::DynamicImage::ImageRgba8(rgba) => rgba,
        other => other.to_rgba8(),
    };
    antiring(&src, &mut rgba, &mask);
    tracing::info!("防振铃耗时: {:.0}ms", t.elapsed().as_secs_f64() * 1000.0);
    image::DynamicImage::ImageRgba8(rgba)
}

/// 强边阈值：shoot_v ≥ 此值的源像素（±1 邻域 cell）才参与防振铃——振铃
/// 幅度 ∝ 边缘对比度，弱边（shoot_v 小）振铃本就 ≤2 级无需处理；强边
/// 像素占比通常 ~5-15%，**边缘定向**把源侧统计与输出侧扫描都收敛到边缘
/// 邻域（shoot_v 图来自锐化链 [`sharpen_step`]，边缘定位零重复扫描）
const SHOOT_RING_THRESHOLD: u8 = 32;

/// anti-ringing（Lanczos 负瓣振铃抑制，放大后处理，**边缘定向**）：把
/// 输出像素 clamp 回源对应 cell 的逐通道 min/max。
///
/// 原理：放大锐度 = 源边缘过渡等比保持，是**域内**属性；振铃 = 负瓣加权
/// 和越过源邻域值域的过冲/欠冲（Gibbs），是**域外**属性——越界值在源图中
/// 不存在，必为插值伪像。clamp 对域内像素恒等映射（边缘过渡/细纹理/
/// 1px 亮线极值全部不动），只剪伪像，锐度无损（mpv zscale antiring 同思路）。
///
/// 实现：shoot_v 掩码标出强边源像素（振铃只发生在强边旁），±1 膨胀为
/// 标记 cell 集——源侧只对标记 cell 统计 3×3 逐通道 min/max（3×3 恰覆盖
/// tap 近邻：线性渐变等合法插值输出严格落在值域内，2×2 会因取整噪声
/// ±1 级误伤），输出侧无标记行整行跳过（O(1) 行和检查）、行内未标记
/// cell 跳过，仅边缘邻域做 6 次比较、越界罕见才 clamp。O(边缘区) 而非
/// O(全图)，热路径零分配
fn antiring(src: &image::RgbaImage, out: &mut image::RgbaImage, shoot_mask: &image::GrayImage) {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (ow, oh) = (out.width() as usize, out.height() as usize);
    if sw == 0 || sh == 0 || ow == 0 || oh == 0 {
        return;
    }
    // 标记 cell 表（shoot_v≥阈值 的 ±1 膨胀）+ 每源行标记数（输出侧行级
    // O(1) 快速跳过）
    let mv = shoot_mask.as_raw();
    let mut marked = vec![0u8; sw * sh];
    let mut rowsum = vec![0u32; sh];
    for sy in 0..sh {
        for sx in 0..sw {
            if mv[sy * sw + sx] >= SHOOT_RING_THRESHOLD {
                for dy in sy.saturating_sub(1)..=(sy + 1).min(sh - 1) {
                    for dx in sx.saturating_sub(1)..=(sx + 1).min(sw - 1) {
                        let i = dy * sw + dx;
                        if marked[i] == 0 {
                            marked[i] = 1;
                            rowsum[dy] += 1;
                        }
                    }
                }
            }
        }
    }
    // 源侧标记 cell 的 3×3 逐通道 min/max（图界 clamp；cmin/cmax 全尺寸
    // 分配但仅标记 cell 被填写，未标记 cell 输出侧不扫不碰）
    let mut cmin = vec![255u8; sw * sh * 3];
    let mut cmax = vec![0u8; sw * sh * 3];
    for sy in 0..sh {
        if rowsum[sy] == 0 {
            continue;
        }
        let y0 = sy.saturating_sub(1);
        let y1 = (sy + 1).min(sh - 1);
        for sx in 0..sw {
            if marked[sy * sw + sx] == 0 {
                continue;
            }
            let x0 = sx.saturating_sub(1);
            let x1 = (sx + 1).min(sw - 1);
            let mut mn = [255u8; 3];
            let mut mx = [0u8; 3];
            for ny in y0..=y1 {
                for nx in x0..=x1 {
                    let p = src[(nx as u32, ny as u32)];
                    for c in 0..3 {
                        mn[c] = mn[c].min(p[c]);
                        mx[c] = mx[c].max(p[c]);
                    }
                }
            }
            let base = (sy * sw + sx) * 3;
            cmin[base..base + 3].copy_from_slice(&mn);
            cmax[base..base + 3].copy_from_slice(&mx);
        }
    }
    let inv_x = sw as f64 / ow as f64;
    let inv_y = sh as f64 / oh as f64;
    // 列映射预计算（ow 项，全部输出行共享）：热路径免逐像素 f64 反算
    let sx_map: Vec<usize> = (0..ow)
        .map(|ox| (((ox as f64 + 0.5) * inv_x) as usize).min(sw - 1))
        .collect();
    let all: &mut [u8] = out; // ImageBuffer DerefMut 直出原始字节（RGBA8 交错的 4 字节/像素）
    for oy in 0..oh {
        let syrow = (((oy as f64 + 0.5) * inv_y) as usize).min(sh - 1);
        if rowsum[syrow] == 0 {
            continue; // 本输出行对应的源行无任何标记 cell，整行免扫
        }
        let sy = syrow * sw * 3;
        let row = &mut all[oy * ow * 4..(oy + 1) * ow * 4];
        // 行内按 cell 分组：min/max 每 cell 只 load 一次，其输出列区间连续
        // 直跑（免逐像素随机 load 与 get_pixel_mut 边界检查）；alpha 直通
        let mut ox = 0;
        while ox < ow {
            let sx = sx_map[ox];
            let end = ox + sx_map[ox..].iter().take_while(|&&s| s == sx).count();
            if marked[syrow * sw + sx] == 0 {
                ox = end;
                continue; // 未标记 cell 不碰（平坦/纹理区，无振铃）
            }
            let ci = sy + sx * 3;
            let (mn0, mn1, mn2) = (cmin[ci], cmin[ci + 1], cmin[ci + 2]);
            let (mx0, mx1, mx2) = (cmax[ci], cmax[ci + 1], cmax[ci + 2]);
            for q in row[ox * 4..end * 4].chunks_exact_mut(4) {
                let (r, g, b) = (q[0], q[1], q[2]);
                if r < mn0 || r > mx0 || g < mn1 || g > mx1 || b < mn2 || b > mx2 {
                    q[0] = r.clamp(mn0, mx0);
                    q[1] = g.clamp(mn1, mx1);
                    q[2] = b.clamp(mn2, mx2);
                }
            }
            ox = end;
        }
    }
}

/// 等比降到 cap 内（快速 box 采样，缩小超一倍时 mipmap 消摩尔纹）
fn thumbnail_cap(img: image::DynamicImage, cap: u32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    if longest <= cap {
        return img;
    }
    let scale = cap as f64 / longest as f64;
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    img.thumbnail(nw, nh)
}

/// 高清档锐化配置（config.ui 透传，放大前在原图分辨率执行）。
/// 独立结构贯穿 decode 链（decode_image/finish_static/build_full），
/// 基础档与测试传 [`SharpenCfg::OFF`]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SharpenCfg {
    /// 总开关（config image_full_sharpen，默认开）
    pub on: bool,
    /// unsharp 强度（config image_full_sharpen_amount，0~2，默认 0.5）：
    /// out = px + amount×(px − blur(px))，越大边缘越脆、光晕与噪点放大越重
    pub amount: f32,
    /// unsharp 半径 σ 像素（config image_full_sharpen_sigma，0.1~4，默认
    /// 1.5）：调大只增强中大尺度边缘（σ 级以下的像素噪点被模糊项抹掉、
    /// 差值趋零），天然抗噪点放大，代价是锐化"颗粒感"变"体积感"
    pub sigma: f32,
    /// 白边抑制曲线查表（config image_full_sharpen_white_edge_curve 折点
    /// 分段线性插值预计算，None = 关闭）：逐点 3×3/5×5/7×7 多尺度亮度方差
    /// 按 shoot.c mode 0 整数组合出 shoot_v ∈ 0..255 标记孤立强边（halo
    /// 源），其正向增强乘 ratio = LUT[shoot_v]——shoot_v 小 ratio≈1（细
    /// 纹理锐度全额保留），到压制拐点后渐进衰减。曲线比单斜率直线观感好
    /// （用户实测直线低段压太多）：低段平缓、强边段陡降；shoot_v 只有
    /// 256 个可能值，任何曲线运行时都收敛为 256 项查表（1 load + 1 add），
    /// 形状自由度零性能代价。ratio 只乘在 d>0 上（亮侧白晕），暗侧压暗
    /// 不受影响
    pub white_edge: Option<[f32; 256]>,
    /// shoot 敏感度增益（config image_full_sharpen_white_edge_gain，默认
    /// 1.0，clamp [1,1024]）：乘在跨尺度方差差（方向保留，不取 abs）上再
    /// 组合 shoot_v。缓坡等边缘的方差差量级小、落在 0..255 低段——增益
    /// 线性展宽，让曲线拐点对得上
    pub shoot_gain: f32,
    /// shoot flank 带外扩半径（config image_full_sharpen_white_edge_dilate，
    /// 0~4 默认 1，0 = 关）：方向保留的方差差响应呈"高亮带在真边缘外侧
    /// 1~2px"——max 膨胀外扩（1 = 外扩 1px）覆盖真边缘线本身；dump/ratio/
    /// 防振铃掩码都吃外扩后值（所见即所得）
    pub shoot_dilate: u32,
    /// 锐化前保边降噪（config image_full_denoise，默认开）：先压平坦区噪点
    /// 再 unsharp，算法由 [`DenoiseAlgo`] 切换（双边 3×3 / 导向滤波）
    pub denoise: bool,
    /// 降噪算法（config image_full_denoise_algo，非法值回退导向并 WARN）
    pub algo: DenoiseAlgo,
    /// 双边降噪颜色容忍度 σc（0~100 亮度级，config image_full_bilateral_strength，
    /// 默认 10，仅 Bilateral 生效）：亮度差小于它的邻点参与平均。越大涂抹越重
    /// （皮肤渐变等弱纹理也被抹掉）、噪点压得越净；越小边缘/毛孔越保，残留
    /// 噪点越多。涂抹感重 → 调小（8~15）；噪点残留 → 调大（25~50）
    pub bilateral_strength: f32,
    /// 导向降噪结构容忍度（0~100 亮度级，config image_full_guided_strength，
    /// 默认 5，仅 Guided 生效）：内部换算 ε = D²，方差 ≪ ε 被平均（压噪）、
    /// ≫ ε 原值保留（边缘/渐变）——与双边 σc 同量纲，同值可比
    pub guided_strength: f32,
    /// 导向滤波窗口半径（1~8，config image_full_guided_radius，默认 1 = 3×3，
    /// 仅 Guided 生效）：越大降噪越强、弱纹理抹除也越强
    pub guided_radius: u32,
    /// debug：锐化 dump shoot 边缘图（config image_full_sharpen_dump_shoot，
    /// 默认关）——shoot_v 直出灰度 bmp 到缓存目录 debug/ 下，验证强边检测
    /// 是否正确
    pub dump_shoot: bool,
    /// debug：锐化 dump 白边抑制曲线查表（config
    /// image_full_sharpen_dump_curve，默认关）——256 行 shoot_v→ratio 文本
    /// 到缓存目录 debug/ 下，验证折点插值生效
    pub dump_curve: bool,
    /// debug：锐化 dump 前后 Y 通道（config image_full_sharpen_dump_y，
    /// 默认关）——sharpen_y_before.bmp（锐化输入，降噪后）/
    /// sharpen_y_after.bmp（锐化输出）两张 8bit 灰度 bmp，逐点核对锐化在
    /// 亮度上做了什么
    pub dump_y: bool,
    /// debug：锐化 dump shoot 白边抑制前后增强量（config
    /// image_full_sharpen_dump_enhance，默认关）——sharpen_delta_before.bmp
    /// （未抑制增强量 amount×d）/ sharpen_delta_after.bmp（抑制后增强量
    /// amount×d×ratio）两张 8bit 灰度 bmp，差异值有正有负**整体 +128 偏移**
    /// （128 = 零点增强），两图相减即白边抑制实际削掉的量
    pub dump_enhance: bool,
    /// 防振铃（config image_full_antiring，默认开）：Lanczos3 负瓣在强边
    /// 两侧产生越出源邻域值域的过冲/欠冲（Gibbs 振铃，×4.5 放大后白线/
    /// 暗线，与 unsharp 白边同向叠加——shoot 治的是放大前、这个治放大时）。
    /// 输出像素 clamp 回源对应 cell 的逐通道 min/max——**域内合法信号
    /// （边缘过渡/细纹理/1px 亮线）恒等映射不动**（锐度是域内属性），越界
    /// 值在源图中不存在必为插值伪像（域外属性），数学上正交不伤锐度
    /// （mpv zscale antiring / FFmpeg 同款思路）。仅放大档（full）生效，
    /// 缩小不处理；独立计时打印
    pub antiring: bool,
}

/// 高清档降噪算法（config image_full_denoise_algo："bilateral"/"guided"）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DenoiseAlgo {
    /// 导向滤波：按结构方差区分"平坦"与"渐变"（He et al. 2010 self-guided），
    /// 噪点压掉而弱纹理/过渡带保留，锯齿感最弱、暗部渐变无分层；O(1)/px
    /// 无 exp，代价是临时内存多于双边（~4 个亮度缓冲）。A/B 实测默认档
    #[default]
    Guided,
    /// 3×3 双边滤波：按亮度差定权重，孤立强噪点抑制更狠（权重直接压零）、
    /// 眼部更锐；渐进弱纹理（皮肤渐变、头发过渡带）会被吸干 → 涂抹感、
    /// 深放大锯齿，大 σ 锐化下暗部投影可能分层（小窗口平滑弱，量化台阶
    /// 残留被拉开）
    Bilateral,
}

impl DenoiseAlgo {
    /// config 字符串解析：trim + 大小写不敏感；非法值回退导向（默认算法）
    /// 并返回 false（调用方 WARN，不静默）
    pub fn parse(s: &str) -> (Self, bool) {
        match s.trim().to_ascii_lowercase().as_str() {
            "guided" => (Self::Guided, true),
            "bilateral" => (Self::Bilateral, true),
            _ => (Self::Guided, false),
        }
    }
}

impl SharpenCfg {
    /// config [image] 段 → 观感增强配置（panels 两处组装点收敛到此；algo 非法
    /// 值 WARN 回退导向，不静默）
    pub fn from_cfg(img: &crate::config::ImageConfig) -> Self {
        let (algo, ok) = DenoiseAlgo::parse(&img.image_full_denoise_algo);
        if !ok {
            tracing::warn!(
                "image_full_denoise_algo = {:?} 无效（仅 bilateral/guided），回退导向",
                img.image_full_denoise_algo
            );
        }
        let (white_edge, ok) = parse_white_edge_curve(&img.image_full_sharpen_white_edge_curve);
        if !ok {
            tracing::warn!(
                "image_full_sharpen_white_edge_curve 折点非法（需 x∈(0,255] 严格递增、y∈[0,1]），回退默认曲线"
            );
        }
        Self {
            on: img.image_full_sharpen,
            amount: img.image_full_sharpen_amount,
            sigma: img.image_full_sharpen_sigma,
            white_edge,
            shoot_gain: img.image_full_sharpen_white_edge_gain.clamp(1.0, 1024.0),
            shoot_dilate: img.image_full_sharpen_white_edge_dilate.clamp(0, 4),
            denoise: img.image_full_denoise,
            algo,
            bilateral_strength: img.image_full_bilateral_strength,
            guided_strength: img.image_full_guided_strength,
            guided_radius: img.image_full_guided_radius,
            dump_shoot: img.image_full_sharpen_dump_shoot,
            dump_curve: img.image_full_sharpen_dump_curve,
            dump_y: img.image_full_sharpen_dump_y,
            dump_enhance: img.image_full_sharpen_dump_enhance,
            antiring: img.image_full_antiring,
        }
    }

    /// 关闭档（基础档解码/测试用）
    pub const OFF: Self = Self {
        on: false,
        amount: 0.0,
        sigma: 0.0,
        white_edge: None,
        shoot_gain: 1.0,
        shoot_dilate: 1,
        denoise: false,
        algo: DenoiseAlgo::Bilateral,
        bilateral_strength: 0.0,
        guided_strength: 0.0,
        guided_radius: 0,
        dump_shoot: false,
        dump_curve: false,
        dump_y: false,
        dump_enhance: false,
        antiring: false,
    };

    /// 降噪/锐化的日志标记（空 = 全关；降噪独立于锐化开关生效，组合四种
    /// 全覆盖；降噪带算法名，A/B 对比时看日志即可分辨本次解码走了哪条链）
    fn tag(&self) -> &'static str {
        match (self.denoise, self.on) {
            (true, true) => match self.algo {
                DenoiseAlgo::Bilateral => " 降噪[双边]+锐化",
                DenoiseAlgo::Guided => " 降噪[导向]+锐化",
            },
            (true, false) => match self.algo {
                DenoiseAlgo::Bilateral => " 降噪[双边]",
                DenoiseAlgo::Guided => " 降噪[导向]",
            },
            (false, true) => " 锐化",
            (false, false) => "",
        }
    }
}

/// debug dump 目录（缓存目录，panels 组装解码参数时 set 一次；OnceLock
/// 全局量同 [`crate::vfs::set_codepage`] 先例——dump 路径不进 SharpenCfg
/// 以保住其 Copy derive，worker 线程 dump 时读取）
static DUMP_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// 设置 debug dump 落盘目录（缓存目录下 debug/ 子目录，dump 文件固定名覆盖）
pub fn set_dump_dir(dir: &std::path::Path) {
    let _ = DUMP_DIR.set(dir.join("debug"));
}

/// debug dump 路径（未 set_dump_dir 或建目录失败返回 None，dump 静默跳过
/// ——debug 功能不影响正常解码）
fn dump_path(name: &str) -> Option<std::path::PathBuf> {
    let dir = DUMP_DIR.get()?;
    std::fs::create_dir_all(dir).ok()?;
    Some(dir.join(name))
}

/// 高清档观感增强链（仅高清档，config image.image_full_sharpen*，默认
/// 开+0.5+1.5）：**YUV 域处理**——解码输出 RGB8 先提取 Y 平面（Rec.601，
/// 与 shoot 方差检测同一定义），可选降噪（算法 [`DenoiseAlgo`] 切换）与
/// unsharp 锐化都只作用于 Y；Δ = 处理后Y − 原Y 等量加回 R/G/B——YUV→RGB
/// 是线性变换，纯 Y 变化 Δ 的逆变换恰为三通道各加 Δ（U/V 不动），数学上
/// 等价于"YUV444 域只改 Y 再转回 RGB"，免物化 U/V 平面。收益：无彩色镶边
/// （色度不锐化不降噪）、blur 只跑 1 通道（锐化步骤提速 ~2/3）。
/// **放大前在原图分辨率**
/// 执行：先把边缘坡度变陡，Lanczos 插值按比例保持陡峭过渡，观感近似
/// "放大后锐化"而成本从放大后千万像素级（~400ms 高斯模糊）降到原图级
/// （2K 图降噪+锐化 ~50ms，被 74MP Lanczos 主耗时掩盖）。实测锐化对深
/// 放大观感增益明显（用户确认默认开）；噪点同步放大由前置降噪对冲。
/// **降噪独立于锐化开关**（image_full_denoise 单独生效，锐化关不连带关
/// 降噪）；两步分别计耗时打日志（tracing info）。行为注记：降噪变为显式
/// Y 域——导向滤波语义不变（本来就只改亮度），双边不再顺带平滑色度
/// （彩色噪点保留，YUV 域语义的正确行为）。
/// 强边白晕由 **shoot 白边抑制**（[`sharpen_step`] 内 [`shoot_ratios`]，
/// 多尺度方差 → 只压正向增强）处理，曲线为空（None）关闭。仅 RGB，alpha
/// 直通。O(n) 后台线程执行。返回 (增强图, shoot_v 边缘掩码)——掩码供
/// 防振铃边缘定向复用（同一份边缘检测结果，避免对放大后大图的重复扫描）
fn sharpen_full(
    img: image::DynamicImage,
    cfg: &SharpenCfg,
) -> (image::DynamicImage, Option<image::GrayImage>) {
    let mut rgba = img.to_rgba8();
    // Y 平面：y0 = 原始亮度（Δ 基准），y = 增强链工作平面
    let y0 = luma_plane(&rgba);
    let mut y = y0.clone();
    // 降噪与锐化两步独立计时：相互遮掩时看日志拆分定位
    let denoise_ms = denoise_step(&mut y, cfg).map(|d| d.as_millis());
    let (sharpen_ms, shoot_mask) = sharpen_step(&mut y, cfg);
    let sharpen_ms = sharpen_ms.map(|d| d.as_millis());
    match (denoise_ms, sharpen_ms) {
        (Some(dn), Some(sh)) => {
            tracing::info!("观感增强耗时: 降噪[{:?}] {dn}ms + 锐化 {sh}ms", cfg.algo)
        }
        (Some(dn), None) => {
            tracing::info!("观感增强耗时: 降噪[{:?}] {dn}ms（锐化关）", cfg.algo)
        }
        (None, Some(sh)) => tracing::info!("观感增强耗时: 锐化 {sh}ms（降噪关）"),
        (None, None) => {}
    }
    // Δ = 处理后Y − 原Y 等量加回 RGB（纯 Y 变化的精确逆变换，色度原样保留；
    // 逐通道 clamp——亮度钳制与通道钳制在 ±1 舍入级内等价）
    for (p, (yn, yo)) in rgba.pixels_mut().zip(y.pixels().zip(y0.pixels())) {
        let delta = f32::from(yn[0]) - f32::from(yo[0]);
        if delta != 0.0 {
            for c in 0..3 {
                p[c] = (f32::from(p[c]) + delta).clamp(0.0, 255.0) as u8;
            }
        }
    }
    (image::DynamicImage::ImageRgba8(rgba), shoot_mask)
}

/// Rec.601 亮度平面提取（u8 取整，与 [`shoot_ratios`] 积分图亮度同一定义）
fn luma_plane(rgba: &image::RgbaImage) -> image::GrayImage {
    image::GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba[(x, y)];
        let l =
            (u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114 + 500) / 1000;
        image::Luma([l as u8])
    })
}

/// 降噪步骤（独立于锐化开关生效）：算法 [`DenoiseAlgo`] 切换，作用于 Y
/// 平面（YUV 域语义：色度不被降噪，None = 关闭）
fn denoise_step(y: &mut image::GrayImage, cfg: &SharpenCfg) -> Option<std::time::Duration> {
    if !cfg.denoise {
        return None;
    }
    let t = std::time::Instant::now();
    *y = match cfg.algo {
        DenoiseAlgo::Bilateral => bilateral_3x3(y, cfg.bilateral_strength.clamp(0.1, 100.0)),
        DenoiseAlgo::Guided => guided_denoise(y, cfg.guided_strength, cfg.guided_radius),
    };
    Some(t.elapsed())
}

/// 锐化步骤（Y 平面 unsharp + shoot 白边抑制 + debug dump）：
/// (None, _) = 关闭；Some 掩码 = shoot_v 边缘图（防振铃定向 + debug dump
/// 共用，need_shoot = dump_shoot || antiring 时才产出）
fn sharpen_step(
    y: &mut image::GrayImage,
    cfg: &SharpenCfg,
) -> (Option<std::time::Duration>, Option<image::GrayImage>) {
    if !cfg.on {
        return (None, None);
    }
    let t = std::time::Instant::now();
    // debug dump 曲线查表（在算出本次实际使用的 LUT 后落盘，验证折点插值）
    if cfg.dump_curve {
        if let Some(lut) = &cfg.white_edge {
            dump_curve_txt(lut);
        }
    }
    let amount = cfg.amount.clamp(0.0, 2.0);
    let sigma = cfg.sigma.clamp(0.1, 4.0);
    // 白边抑制（shoot，见 [`shoot_ratios`]）：逐点多尺度方差标出孤立强边
    // （halo 源），其正向增强按曲线 LUT[shoot_v] 衰减。取代旧 ±20 过冲钳制
    // ——钳差值把强边锐感一起削平（用户实测无感弃用）；ratio 只乘在 d>0
    // （亮侧白晕），负向压暗与细纹理（shoot_v≈0，ratio≈1）全额保留。
    // curve 空（None）关闭（省方差计算）。shoot_v 图同时是防振铃的边缘
    // 定向掩码——锐化与防振铃共用同一份边缘检测结果，不重复扫描
    let need_shoot = cfg.dump_shoot || cfg.antiring;
    let (ratios, shoot_img) = match &cfg.white_edge {
        Some(lut) => shoot_ratios(y, lut, cfg.shoot_gain, cfg.shoot_dilate, need_shoot),
        None => (Vec::new(), None),
    };
    // debug dump shoot 边缘图（shoot_v 直出灰度，白 = 强边、黑 = 细纹理）
    if let Some(g) = &shoot_img {
        dump_gray_bmp(g, "sharpen_shoot_dump.bmp", "锐化 shoot 边缘图");
    }
    // debug dump 锐化输入 Y（降噪后）
    if cfg.dump_y {
        dump_gray_bmp(y, "sharpen_y_before.bmp", "锐化输入 Y");
    }
    let blurred = image::imageops::blur(y, sigma);
    // debug dump 缓冲：增强量差异值（8bit 有符号 → **+128 偏移**，128 = 零点
    // 增强）——before = amount×d（未抑制）、after = amount×d×ratio（抑制后）
    let (w, h) = y.dimensions();
    let n = (w * h) as usize;
    let mut delta_before = cfg.dump_enhance.then(|| vec![0u8; n]);
    let mut delta_after = cfg.dump_enhance.then(|| vec![0u8; n]);
    for (i, (p, b)) in y.pixels_mut().zip(blurred.pixels()).enumerate() {
        let ratio = if ratios.is_empty() { 1.0 } else { ratios[i] };
        let yv = f32::from(p[0]);
        let d = yv - f32::from(b[0]);
        let e = if d > 0.0 {
            amount * ratio * d
        } else {
            amount * d
        };
        if let Some(buf) = &mut delta_before {
            buf[i] = (amount * d + 128.0).clamp(0.0, 255.0) as u8;
        }
        if let Some(buf) = &mut delta_after {
            buf[i] = (e + 128.0).clamp(0.0, 255.0) as u8;
        }
        let v = (yv + e).clamp(0.0, 255.0);
        p[0] = v as u8;
    }
    if let Some(buf) = delta_before {
        dump_gray_bmp(
            &gray_from(buf, w, h),
            "sharpen_delta_before.bmp",
            "锐化未抑制增强量",
        );
    }
    if let Some(buf) = delta_after {
        dump_gray_bmp(
            &gray_from(buf, w, h),
            "sharpen_delta_after.bmp",
            "锐化抑制后增强量",
        );
    }
    // debug dump 锐化输出 Y（最终增强后的亮度平面）
    if cfg.dump_y {
        dump_gray_bmp(y, "sharpen_y_after.bmp", "锐化输出 Y");
    }
    (Some(t.elapsed()), shoot_img)
}

/// 缓冲 → 灰度图（dump 辅助；缓冲尺寸与 (w,h) 自洽由调用方保证）
fn gray_from(buf: Vec<u8>, w: u32, h: u32) -> image::GrayImage {
    image::GrayImage::from_raw(w, h, buf).expect("缓冲与尺寸自洽")
}

/// debug dump：灰度图 bmp（缓存目录 debug/ 下固定名覆盖；失败静默 WARN，
/// debug 功能不影响正常解码）
fn dump_gray_bmp(img: &image::GrayImage, name: &str, desc: &str) {
    let Some(path) = dump_path(name) else { return };
    match img.save(&path) {
        Ok(()) => tracing::info!("debug dump: {desc} {:?}", path),
        Err(e) => tracing::warn!("debug dump {desc} 失败: {e}"),
    }
}

/// debug dump：白边抑制曲线查表 256 行（缓存目录 debug/sharpen_curve_dump.txt，
/// shoot_v→ratio，验证折点分段线性插值生效；固定名覆盖）
fn dump_curve_txt(lut: &[f32; 256]) {
    let Some(path) = dump_path("sharpen_curve_dump.txt") else {
        return;
    };
    let mut s = String::with_capacity(256 * 16);
    s.push_str("# 白边抑制曲线查表 dump：shoot_v -> ratio（折点分段线性插值）\n");
    for (v, r) in lut.iter().enumerate() {
        s.push_str(&format!("{v} {:.4}\n", *r));
    }
    match std::fs::write(&path, s) {
        Ok(()) => tracing::info!("debug dump: 白边曲线查表 {:?}", path),
        Err(e) => tracing::warn!("debug dump curve 失败: {e}"),
    }
}

/// k 尺度窗口方差（内部区固定偏移四角定址）：窗口和先收 u32（sum ≤ 255×49、
/// sumsq ≤ 65025×49）再转 f32——u64→f32 无 AVX-512 时是多周期软序列
#[inline(always)]
fn window_var(sum: &[u64], sum2: &[u64], base: usize, stride: usize, k: usize, inv: f32) -> f32 {
    let br = base + (k + 1) * stride + (k + 1);
    let tl = base - k * stride - k;
    let tr = base - k * stride + (k + 1);
    let bl = base + (k + 1) * stride - k;
    let mean = (sum[br] + sum[tl] - sum[tr] - sum[bl]) as u32 as f32 * inv;
    let q = (sum2[br] + sum2[tl] - sum2[tr] - sum2[bl]) as u32 as f32 * inv;
    q - mean * mean
}

/// shoot.c mode 0 整数组合：跨尺度方差差（**方向保留，不取 abs**——实测
/// abs 响应毛躁、点亮细结构两侧反转差值导致结构紊乱；方向保留只响应
/// v7>v3 的宽边 flank，边缘带干净）乘敏感度增益后转 i32 CLIP 0..255，
/// 组合出 shoot_v ∈ 0..255。增益语义：缓坡等边缘的方差差量级小（如
/// 4 级/px 坡 v7−v3≈53），1.0 时落在 0..255 低段够不到曲线拐点——gain
/// 线性展宽让拐点对得上，1.0 = 最接近原始 shoot.c 语义。全程无 sqrt
/// 无除法——shoot 检测不吃精度，整数化零观感损失
#[inline(always)]
fn shoot_v_of(v3: f32, v5: f32, v7: f32, gain: f32) -> usize {
    let s1 = ((((v7 - v3) * gain) as i32) / 2).clamp(0, 255);
    let s2 = (((v7 - v5) * gain) as i32).clamp(0, 255);
    (((s1 + s2 + 1) / 2).clamp(0, 255)) as usize
}

/// config 曲线解析：折点校验（x ∈ (0,255] 严格递增、y ∈ [0,1]），非法回退
/// 默认曲线并返回 false（调用方 WARN，不静默，同 [`DenoiseAlgo::parse`]
/// 先例）；空表 = 关闭（连方差计算都跳过）返回 (None, true)
fn parse_white_edge_curve(curve: &[(f32, f32)]) -> (Option<[f32; 256]>, bool) {
    use crate::config::DEFAULT_WHITE_EDGE_CURVE;
    if curve.is_empty() {
        return (None, true);
    }
    let valid = curve[0].0 > 0.0
        && curve
            .iter()
            .all(|&(x, y)| (0.0..=255.0).contains(&x) && (0.0..=1.0).contains(&y))
        && curve.windows(2).all(|w| w[0].0 < w[1].0);
    if valid {
        (Some(build_white_edge_lut(curve)), true)
    } else {
        (Some(build_white_edge_lut(&DEFAULT_WHITE_EDGE_CURVE)), false)
    }
}

/// 折点曲线 → 256 项 ratio 查表：隐含首点 [0, 1]（shoot_v 低于首折点从 1
/// 渐变到首点 y——细纹理轻压不误伤），折点间**分段线性插值**，折点精确
/// 落值，末点后平坦 = 末点 y。前提：curve 非空且已通过校验（x>0 严格递增）
fn build_white_edge_lut(curve: &[(f32, f32)]) -> [f32; 256] {
    let mut lut = [1.0f32; 256];
    let (mut px, mut py) = (0.0f32, 1.0f32); // 隐含首点 [0, 1]
    let mut vi = 0usize; // 下一个待填的 shoot_v
    for &(x, y) in curve {
        let y = y.clamp(0.0, 1.0);
        let xi = (x.round() as usize).clamp(1, 255);
        // [vi, xi) 区间从 (px,py) 到 (x,y) 线性插值（x−px>0 由严格递增保证）
        while vi < xi {
            let t = (vi as f32 - px) / (x - px);
            lut[vi] = (py + (y - py) * t).clamp(0.0, 1.0);
            vi += 1;
        }
        lut[xi] = y; // 折点精确落值
        vi = xi + 1;
        px = x;
        py = y;
    }
    while vi <= 255 {
        lut[vi] = py; // 末点后平坦延伸
        vi += 1;
    }
    lut[0] = 1.0; // shoot_v=0（细纹理）恒不抑制，机制保证
    lut
}

/// shoot 白边抑制比例表（每像素 0~1，1 = 不抑制）：多尺度亮度方差（3×3/
/// 5×5/7×7）逐点求跨尺度方差差，按相机 ISP 库 shoot.c get_shoot_value
/// mode 0 整数组合出 shoot_v ∈ 0..255——孤立强边周围 7×7 窗口方差显著高于
/// 局部 3×3 → 方差差大 → shoot_v 大；细纹理/噪点各尺度方差相近、强边正
/// 中心 var3≈var7 → shoot_v≈0。ratio 由曲线查表给出（[`build_white_edge_lut`]，
/// `lut[shoot_v]`），shoot_v 越大 ratio 越小。
///
/// 性能：三尺度 6 个窗口量（各尺度 sum/sumsq）全部从**同一份积分图** O(1)
/// 查表复用——前缀和一次建图（1 趟 Y 读 + 2 个 u64 表写），替代按尺度
/// 各跑 box_filter 的 12 趟全图扫描 + 6 次临时缓冲分配。窗口边界 clamp 到
/// 图界，输入 = Y 平面（YUV 域增强链的亮度，u8；方差检测对 ±1 亮度不
/// 敏感），u64 前缀和精确无损（sumsq ≤ 65025×像素数 ≪ u64 溢出）。
/// `need_shoot` 时同步产出
/// shoot_v 灰度图（debug dump 验证强边检测 + **防振铃边缘定向掩码**共用；
/// 关闭时不分配零开销）
fn shoot_ratios(
    y_img: &image::GrayImage,
    lut: &[f32; 256],
    gain: f32,
    dilate: u32,
    need_shoot: bool,
) -> (Vec<f32>, Option<image::GrayImage>) {
    let (w, h) = (y_img.width() as usize, y_img.height() as usize);
    let stride = w + 1;
    // 亮度及其平方的前缀和积分图：(w+1)×(h+1) 首行/首列全 0，免边界分支
    let mut sum = vec![0u64; stride * (h + 1)];
    let mut sum2 = vec![0u64; stride * (h + 1)];
    for y in 0..h {
        let (cur, prev) = ((y + 1) * stride, y * stride);
        let (mut rs, mut rs2) = (0u64, 0u64);
        for x in 0..w {
            let l = u64::from(y_img[(x as u32, y as u32)][0]);
            rs += l;
            rs2 += l * l;
            sum[cur + x + 1] = sum[prev + x + 1] + rs;
            sum2[cur + x + 1] = sum2[prev + x + 1] + rs2;
        }
    }
    // 三尺度方差 → shoot_v 字节图（膨胀/查表/dump 三消费者共用一份）
    let inv_n = [1.0f32 / 9.0, 1.0 / 25.0, 1.0 / 49.0];
    let mut sv_map = vec![0u8; w * h];
    // 内部区：k 尺度窗口 [x−k, x+k+1)×[y−k, y+k+1)，四角定址固定偏移
    // （br/tl/tr/bl），像素数倒数预算，零除法零边界分支零逐像素 if——
    // 热循环无任何分叉，与手写基准循环结构一致（实测 closure + 逐像素
    // 分支版本合成段慢一倍）
    for y in 3..h.saturating_sub(3) {
        for x in 3..w.saturating_sub(3) {
            let base = y * stride + x;
            let v3 = window_var(&sum, &sum2, base, stride, 1, inv_n[0]);
            let v5 = window_var(&sum, &sum2, base, stride, 2, inv_n[1]);
            let v7 = window_var(&sum, &sum2, base, stride, 3, inv_n[2]);
            sv_map[y * w + x] = shoot_v_of(v3, v5, v7, gain) as u8;
        }
    }
    // 边框窄带（3px）：窗口 clamp 到图界，通用四角查表（冷路径；四条带各
    // 自直跑，免全图扫描；h/w < 6 时带重叠，同值重复写无害）
    let mut band = |y: usize, x: usize| {
        let var = |r: usize| -> f32 {
            let xa = x.saturating_sub(r);
            let ya = y.saturating_sub(r);
            let xb = (x + r + 1).min(w);
            let yb = (y + r + 1).min(h);
            let inv = 1.0 / ((xb - xa) * (yb - ya)) as f32;
            let (a, b) = (ya * stride, yb * stride);
            let mean = (sum[b + xb] + sum[a + xa] - sum[a + xb] - sum[b + xa]) as u32 as f32 * inv;
            let q = (sum2[b + xb] + sum2[a + xa] - sum2[a + xb] - sum2[b + xa]) as u32 as f32 * inv;
            q - mean * mean
        };
        sv_map[y * w + x] = shoot_v_of(var(1), var(2), var(3), gain) as u8;
    };
    for y in 0..h.min(3) {
        for x in 0..w {
            band(y, x);
        }
    }
    for y in h.saturating_sub(3)..h {
        for x in 0..w {
            band(y, x);
        }
    }
    for y in 3..h.saturating_sub(3) {
        for x in 0..w.min(3) {
            band(y, x);
        }
        for x in w.saturating_sub(3)..w {
            band(y, x);
        }
    }
    // flank 带外扩：方向保留的方差差响应天然呈"高亮带在真边缘外侧 1~2px"
    // （边缘正中 v3≈v7≈v5 差值收敛为低值）——小半径 max 膨胀把干净的
    // flank 带向外扩（1 = 外扩 1px，2 = 外扩 2px），覆盖真边缘线本身。
    // 0 = 关（原始逐点语义）
    if dilate > 0 {
        max_dilate(&mut sv_map, w, h, dilate as usize);
    }
    // 曲线查表（膨胀后的 sv 驱动 ratio 与 dump——dump 显示的就是实际
    // 参与抑制的值，验证覆盖时所见即所得）
    let mut ratios = vec![0f32; w * h];
    for (r, &sv) in ratios.iter_mut().zip(sv_map.iter()) {
        *r = lut[sv as usize];
    }
    let shoot_dump = need_shoot.then(|| {
        let mut g = image::GrayImage::new(w as u32, h as u32);
        let raw: &mut [u8] = &mut g;
        raw.copy_from_slice(&sv_map);
        g
    });
    (ratios, shoot_dump)
}

/// 分离式最大值滤波（水平一趟 + 垂直一趟，窗口 clamp 图界）——两趟各
/// O(n·(2r+1))，n 为像素数；r≤4 的窄窗口远优于逐像素方形扫描。把 flank
/// 高亮带向外扩 r 像素（1 = 外扩 1px），覆盖真边缘线本身
fn max_dilate(map: &mut [u8], w: usize, h: usize, r: usize) {
    let mut tmp = vec![0u8; map.len()];
    for y in 0..h {
        let row = &map[y * w..(y + 1) * w];
        let out = &mut tmp[y * w..(y + 1) * w];
        for (x, o) in out.iter_mut().enumerate() {
            let lo = x.saturating_sub(r);
            let hi = (x + r + 1).min(w);
            *o = row[lo..hi].iter().max().copied().unwrap_or(0);
        }
    }
    for x in 0..w {
        for y in 0..h {
            let lo = y.saturating_sub(r);
            let hi = (y + r + 1).min(h);
            let m = (lo..hi).map(|yy| tmp[yy * w + x]).max().unwrap_or(0);
            map[y * w + x] = m;
        }
    }
}

/// 3×3 双边滤波（保边降噪，锐化前置）：空间权重固定 3×3 高斯 σs=1.0，
/// 颜色权重按中心与邻点**亮度差的指数衰减**——`sigma_c`（亮度级 0~100）
/// 是颜色容忍度：亮度差小于它的邻点参与平均。sigma_c 越大平均越猛，皮肤
/// 渐变/毛孔等弱纹理也被抹掉（涂抹感）；越小只压最细噪点、弱纹理全保。
/// YUV 域语义：作用于 Y 平面，色度不被降噪（彩色噪点保留）。中心权重恒
/// 1.0 无除零；O(n·9) 纯 Rust，2K 图 ~10ms 后台执行
fn bilateral_3x3(img: &image::GrayImage, sigma_c: f32) -> image::GrayImage {
    // 3×3 高斯 σs=1.0：exp(-0.5/2σs²)=0.6065（正交）、exp(-1/2σs²)=0.3679（对角）
    const SW: [f32; 9] = [
        0.3679, 0.6065, 0.3679, 0.6065, 1.0, 0.6065, 0.3679, 0.6065, 0.3679,
    ];
    let (w, h) = (img.width() as i64, img.height() as i64);
    let src = img.as_raw();
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) as usize;
            let lc = f32::from(src[idx]);
            let (mut acc, mut wsum) = (0f32, 0f32);
            let mut k = 0;
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    // 边界钳位到图内（边缘邻点重复采样，简单且够用）
                    let (ny, nx) = ((y + dy).clamp(0, h - 1), (x + dx).clamp(0, w - 1));
                    let j = (ny * w + nx) as usize;
                    let d = f32::from(src[j]) - lc;
                    let wgt = SW[k] * (-(d * d) / (2.0 * sigma_c * sigma_c)).exp();
                    acc += wgt * f32::from(src[j]);
                    wsum += wgt;
                    k += 1;
                }
            }
            out.push((acc / wsum).clamp(0.0, 255.0) as u8);
        }
    }
    image::GrayImage::from_raw(img.width(), img.height(), out).expect("缓冲与尺寸自洽")
}

/// 导向滤波（self-guided 灰度版，保边降噪可选项）：引导图 = 自身亮度 I。
/// O(1)/像素——窗口半径无关的滑动 box 均值 ×4 遍 + 线性算术，全程无 exp
/// （双边 3×3 每像素 9 次 exp 是其性能大头），2K 图 r=2 约 40ms 后台执行、
/// 与双边 3×3（~30ms）同量级。峰值临时内存 ~4 个 f32 亮度缓冲（2K 图
/// ~60MB、7000P 相机图 ~520MB，后台解码线程瞬时分配用完即释放）
///
/// 算法（He et al. 2010，I=P self-guided）：
///   mean_I = box(I)；corr = box(I·I)；cov = corr − mean_I²
///   a = cov/(cov+ε)；b = (1−a)·mean_I；q = box(a)·I + box(b)
/// ε 是"结构方差容忍度"：局部方差 ≪ ε 的区域（平坦/弱纹理）q→局部均值
/// （噪点被平均）；方差 ≫ ε 的区域（强边缘/渐变）a→0、b→I、q→I 原值保留。
/// 与双边 σc 的换算：亮度差容忍 D 级 ↔ 方差容忍 D²，故 ε = strength²，
/// 两算法 strength 同值即可公平对比（见 config image_full_guided_strength）。
/// 渐进渐变（皮肤、头发过渡带）局部方差远小于强边缘 → 不被吸干，这是相对
/// 双边的核心增益；孤立强噪点抑制弱于双边（噪点抬高了局部方差，a 不趋零）
/// ——噪点类型按图源 A/B 选择
///
/// 输出 = 平滑后的 Y（YUV 域语义：只改亮度，色度原样保留）。边缘 clamp 重复采样
fn guided_denoise(img: &image::GrayImage, strength: f32, radius: u32) -> image::GrayImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let r = radius.clamp(1, 8) as usize;
    let eps = strength.clamp(0.1, 100.0) * strength.clamp(0.1, 100.0);
    // 输入即 Y 平面（f32 视图）
    let lum: Vec<f32> = img.as_raw().iter().map(|v| f32::from(*v)).collect();
    let mut mean_i = box_filter(&lum, w, h, r);
    // corr = box(I·I) − mean_I² → 原地换算成 a = cov/(cov+ε)
    let sq: Vec<f32> = lum.iter().map(|v| v * v).collect();
    let mut a = box_filter(&sq, w, h, r);
    drop(sq);
    for i in 0..a.len() {
        let cov = a[i] - mean_i[i] * mean_i[i];
        a[i] = cov / (cov + eps);
    }
    // b = (1−a)·mean_I 原地写进 mean_i（mean_I 已消费完）
    for i in 0..mean_i.len() {
        mean_i[i] = (1.0 - a[i]) * mean_i[i];
    }
    let mean_a = box_filter(&a, w, h, r);
    drop(a);
    let mean_b = box_filter(&mean_i, w, h, r);
    // q = mean_a·I + mean_b → 逐像素回写 Y（无需 q 缓冲）
    let out = img
        .as_raw()
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let delta = mean_a[i] * lum[i] + mean_b[i] - lum[i];
            (f32::from(*v) + delta).clamp(0.0, 255.0) as u8
        })
        .collect();
    image::GrayImage::from_raw(img.width(), img.height(), out).expect("缓冲与尺寸自洽")
}

/// 一维滑动 box 均值两段式（水平→垂直），边缘 clamp 重复采样（窗口恒
/// (2r+1) 个样本、除数恒窗口大小，等效边缘延拓）。每像素 O(1)（加新列减
/// 旧列）与半径无关，O(w) 列和临时；两段式共分配 2 个 (w×h) 缓冲
fn box_filter(src: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let win = (2 * r + 1) as f32;
    // 水平段：行内滑窗（原地输出到 tmp）
    let mut tmp = vec![0f32; w * h];
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let out = &mut tmp[y * w..(y + 1) * w];
        let mut sum = 0f32;
        for j in 0..(2 * r + 1) {
            let x = (j as isize - r as isize).clamp(0, w as isize - 1) as usize;
            sum += row[x];
        }
        for x in 0..w {
            out[x] = sum / win;
            // 推进：x→x+1 加右新列、减左退列（均 clamp，重复列加减抵消）
            let add = (x + r + 1).min(w - 1);
            let sub = (x as isize - r as isize).clamp(0, w as isize - 1) as usize;
            sum += row[add] - row[sub];
        }
    }
    // 垂直段：列和增量滑窗（原地输出到 dst）
    let mut dst = vec![0f32; w * h];
    let mut colsum = vec![0f32; w];
    for j in 0..(2 * r + 1) {
        let y = (j as isize - r as isize).clamp(0, h as isize - 1) as usize;
        for (x, cs) in colsum.iter_mut().enumerate() {
            *cs += tmp[y * w + x];
        }
    }
    for y in 0..h {
        let out = &mut dst[y * w..(y + 1) * w];
        out.copy_from_slice(&colsum);
        for v in out.iter_mut() {
            *v /= win;
        }
        let add = (y + r + 1).min(h - 1);
        let sub = (y as isize - r as isize).clamp(0, h as isize - 1) as usize;
        for (x, cs) in colsum.iter_mut().enumerate() {
            *cs += tmp[add * w + x] - tmp[sub * w + x];
        }
    }
    dst
}

/// DynamicImage → egui ColorImage（RGBA，unmultiplied——egui 内部预乘）
fn color_image(img: image::DynamicImage) -> egui::ColorImage {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
}

// ---- 缩放/平移几何（纯函数，UI 层复用 + 单测） ----

/// 适应窗口的缩放倍率：纹理按此倍率显示恰好装进视口（保持纵横比）
pub fn fit_scale(viewport: egui::Rect, native: egui::Vec2) -> f32 {
    let (vw, vh) = (viewport.width().max(1.0), viewport.height().max(1.0));
    (vw / native.x).min(vh / native.y)
}

/// 缩放后的平移量：保持屏幕锚点 `rel`（相对视口中心）下的图像点不动。
/// `pan` = 图像中心相对视口中心的偏移；`rel = 锚点 - 视口中心`
pub fn zoom_pan(pan: egui::Vec2, cur_zoom: f32, new_zoom: f32, rel: egui::Vec2) -> egui::Vec2 {
    let cz = cur_zoom.max(f32::EPSILON);
    // 锚点下的图像坐标（相对图像中心，单位 = 原生像素）
    let ip = (rel - pan) / cz;
    // 新倍率下让同一图像坐标仍落在锚点下
    rel - ip * new_zoom
}

/// 平移边界：图大于视口的轴，图像边缘不得露进视口（拖不丢，画面总覆盖该轴）；
/// 图不大于视口的轴归零居中（fit/矮图拖拽自动回位，标准看图行为）
pub fn clamp_pan(pan: egui::Vec2, disp: egui::Vec2, viewport: egui::Rect) -> egui::Vec2 {
    let mut p = pan;
    let c = viewport.center();
    // 图覆盖视口的条件：中心 ∈ [right-half, left+half]（disp ≥ 宽时区间有效）
    if disp.x <= viewport.width() {
        p.x = 0.0;
    } else {
        let half = disp.x / 2.0;
        p.x = (c.x + p.x).clamp(viewport.right() - half, viewport.left() + half) - c.x;
    }
    if disp.y <= viewport.height() {
        p.y = 0.0;
    } else {
        let half = disp.y / 2.0;
        p.y = (c.y + p.y).clamp(viewport.bottom() - half, viewport.top() + half) - c.y;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试调度器：默认 IoConfig（io_parallel_max_num=1，让路全开）
    fn test_sched() -> std::sync::Arc<crate::iosched::IoScheduler> {
        crate::iosched::IoScheduler::new(&crate::config::IoConfig::default())
    }

    fn test_cfg(image_texture: bool) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.image.image_texture = image_texture;
        c
    }

    /// 开启锐化的测试配置（denoise 关：降噪滤波对合成阶跃图基本不动，
    /// 测试只验 unsharp 行为；白边抑制用默认曲线）
    fn sharpen_on(amount: f32) -> SharpenCfg {
        SharpenCfg {
            on: true,
            amount,
            sigma: 1.0,
            white_edge: Some(build_white_edge_lut(
                &crate::config::DEFAULT_WHITE_EDGE_CURVE,
            )),
            shoot_gain: 1.0,
            shoot_dilate: 1,
            denoise: false,
            algo: DenoiseAlgo::Bilateral,
            bilateral_strength: 15.0,
            guided_strength: 15.0,
            guided_radius: 2,
            dump_shoot: false,
            dump_curve: false,
            dump_y: false,
            dump_enhance: false,
            antiring: false,
        }
    }

    #[test]
    fn antiring_振铃钳回源值域() {
        // 灰底 128 + 白块 255（强边对比 127）：Lanczos ×4 放大后强边旁必有
        // 越界振铃（欠冲 <128 / 过冲 >255→被 fir 输出 clamp 到 255 后仍高于
        // 源 cell 值域判断需要），antiring 后所有像素钳回源全局值域 [128,255]
        let mut src = image::RgbaImage::new(16, 16);
        for y in 0..16u32 {
            for x in 0..16u32 {
                let v = if (4..12).contains(&x) && (4..12).contains(&y) {
                    255
                } else {
                    128
                };
                src.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        let out = fir_resize(image::DynamicImage::ImageRgba8(src.clone()), 64, 64).to_rgba8();
        let mut fixed = out.clone();
        // 掩码来自锐化链同一份 shoot_v 边缘检测（防振铃边缘定向的真实链路）
        let lut = build_white_edge_lut(&crate::config::DEFAULT_WHITE_EDGE_CURVE);
        let (_, mask) = shoot_ratios(&luma_plane(&src), &lut, 1.0, 1, true);
        assert!(mask.is_some(), "棋盘强边应产出掩码");
        antiring(&src, &mut fixed, &mask.unwrap());
        assert_ne!(out.as_raw(), fixed.as_raw(), "强边振铃应被 clamp 修正");
        assert!(
            fixed
                .pixels()
                .all(|p| p[0] >= 128 && p[1] >= 128 && p[2] >= 128),
            "欠冲振铃应钳回源值域下界 128"
        );
        // 无强边的线性渐变：shoot_v 全部低于阈值 → 掩码全空 → 完全恒等
        // （边缘定向的整行/整 cell 跳过路径验证）
        let mut ramp = image::RgbaImage::new(16, 16);
        for y in 0..16u32 {
            for x in 0..16u32 {
                let v = (x * 16) as u8;
                ramp.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        let r_out = fir_resize(image::DynamicImage::ImageRgba8(ramp.clone()), 64, 64).to_rgba8();
        let mut r_fixed = r_out.clone();
        let (_, r_mask) = shoot_ratios(&luma_plane(&ramp), &lut, 1.0, 1, true);
        antiring(&ramp, &mut r_fixed, &r_mask.unwrap());
        // 陡渐变（16 级/px）的方差跨尺度差同样大 → shoot_v≥阈值被标记
        // （shoot 检测的是方差梯度非孤立强边）——但渐变无 unsharp 白边
        // 问题，且 fir 的 Lanczos3 对陡渐变自身有 ±2 级实现误差（f32 权重
        // 精度限制），clamp 把越界处拉回源 cell 值域 = 纠偏向真实线性值，
        // 非误伤：clamp 量 = fir 越界量，≤2 级不可感知
        let maxdiff = r_out
            .pixels()
            .zip(r_fixed.pixels())
            .map(|(a, b)| {
                (0..3)
                    .map(|c| (a[c] as i32 - b[c] as i32).abs())
                    .max()
                    .unwrap()
            })
            .max()
            .unwrap();
        assert!(
            maxdiff <= 2,
            "渐变 clamp 幅度应仅限 fir 实现误差级别: {maxdiff}"
        );
    }

    #[test]
    fn box_filter_滑动均值与朴素参考一致() {
        // 推进与可分离两段式（水平⊗垂直 = 2D box）的正确性
        let (w, h, r) = (17, 13, 2);
        let src: Vec<f32> = (0..w * h).map(|i| (i * 37 % 91) as f32 - 45.0).collect();
        let got = box_filter(&src, w, h, r);
        for y in 0..h {
            for x in 0..w {
                let mut s = 0f32;
                for dy in -(r as isize)..=(r as isize) {
                    for dx in -(r as isize)..=(r as isize) {
                        let ny = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                        let nx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                        s += src[ny * w + nx];
                    }
                }
                let want = s / ((2 * r + 1) * (2 * r + 1)) as f32;
                assert!(
                    (got[y * w + x] - want).abs() < 1e-3,
                    "({x},{y}) got={} want={want}",
                    got[y * w + x]
                );
            }
        }
    }

    #[test]
    fn guided_denoise_压噪与保边() {
        // 左右平坦区叠 ±10 确定性噪点，中带为无噪强边（与两侧差 120）：
        // ε = strength² = 225 ≫ 噪点窗口方差 → 平坦区平均压噪；强边窗口
        // 方差 ~3600 ≫ ε → a→1、q→I 原值保留（YUV 域：输入即 Y 平面）
        let (w, h) = (48, 16);
        let mut img = image::GrayImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let base: i16 = if (20..28).contains(&x) { 200 } else { 80 };
                let noise: i16 = if (20..28).contains(&x) {
                    0
                } else {
                    ((x * 7 + y * 13) % 23) as i16 - 11
                };
                let v = (base + noise).clamp(0, 255) as u8;
                img.put_pixel(x, y, image::Luma([v]));
            }
        }
        let lum = |im: &image::GrayImage, x: u32, y: u32| f32::from(im[(x, y)][0]);
        let out = guided_denoise(&img, 15.0, 2);
        // 强边竖带基本保留：导向滤波的 box(a)/box(b) 二次平滑允许边缘
        // ~几个百分点的过渡（区别于双边的权重压零硬保留），但绝不允许
        // 被"吸干"——竖带亮度必须仍接近 200（差 ≤8 / 原差 120）
        for y in 0..h {
            for x in 20..28 {
                let d = lum(&out, x, y) - lum(&img, x, y);
                assert!(d >= -8.0 && d <= 2.0, "强边被改 ({x},{y}) Δ={d}");
            }
        }
        // 平坦区噪点能量（相对区域均值的均方差）下降 >60%
        let noise_energy = |im: &image::GrayImage| {
            let mut sum = 0f64;
            let mut sum2 = 0f64;
            let mut n = 0u64;
            for y in 0..h {
                for x in 0..w {
                    if (20..28).contains(&x) {
                        continue;
                    }
                    let l = lum(im, x, y) as f64;
                    sum += l;
                    sum2 += l * l;
                    n += 1;
                }
            }
            (sum2 / n as f64 - (sum / n as f64).powi(2)).sqrt()
        };
        assert!(
            noise_energy(&out) < noise_energy(&img) * 0.4,
            "噪点未被压住: {} → {}",
            noise_energy(&img),
            noise_energy(&out)
        );
    }

    #[test]
    fn denoise_algo_解析回退与标记() {
        assert_eq!(DenoiseAlgo::parse("guided"), (DenoiseAlgo::Guided, true));
        assert_eq!(DenoiseAlgo::parse(" Guided "), (DenoiseAlgo::Guided, true));
        assert_eq!(
            DenoiseAlgo::parse("bilateral"),
            (DenoiseAlgo::Bilateral, true)
        );
        // 非法值回退导向（默认算法）+ 标记 false（调用方 WARN 不静默）
        assert_eq!(DenoiseAlgo::parse("nlmeans"), (DenoiseAlgo::Guided, false));
        // tag 降噪标记带算法名：A/B 对比时看解码日志即可分辨走了哪条链
        let mut c = sharpen_on(0.6);
        c.denoise = true;
        c.algo = DenoiseAlgo::Guided;
        assert_eq!(c.tag(), " 降噪[导向]+锐化");
        c.algo = DenoiseAlgo::Bilateral;
        assert_eq!(c.tag(), " 降噪[双边]+锐化");
    }

    #[test]
    fn sharpen_full_降噪算法分发真实生效() {
        // 噪声图 + amount 0（unsharp 恒等）→ 输出差异只来自降噪算法本身，
        // 证明 match 分发没有写错支路
        let (w, h) = (24, 24);
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = ((x * 7 + y * 13) % 91 + 80).clamp(0, 255) as u8;
                img.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        let mut cfg = sharpen_on(0.0);
        cfg.denoise = true;
        let src = image::DynamicImage::ImageRgba8(img);
        cfg.algo = DenoiseAlgo::Guided;
        let guided = sharpen_full(src.clone(), &cfg).0.to_rgba8();
        cfg.algo = DenoiseAlgo::Bilateral;
        let bilateral = sharpen_full(src, &cfg).0.to_rgba8();
        let diff = guided
            .pixels()
            .zip(bilateral.pixels())
            .filter(|(a, b)| a[0] != b[0])
            .count();
        assert!(
            diff > w as usize * h as usize / 10,
            "两算法输出几乎相同，分发可疑: diff={diff}"
        );
    }

    #[test]
    fn wants_texture_路由判定() {
        let cfg = test_cfg(true);
        // 静态图与动图（gif/webp）全走纹理——mpv 只保留视频
        assert!(wants_texture(&cfg, "page.jpg"));
        assert!(wants_texture(&cfg, "PAGE.PNG"));
        assert!(wants_texture(&cfg, "a.bmp"));
        assert!(wants_texture(&cfg, "anim.gif"));
        assert!(wants_texture(&cfg, "anim.webp"));
        assert!(wants_texture(&cfg, "static.webp"));
        // 非图片
        assert!(!wants_texture(&cfg, "video.mp4"));
        // 配置关闭
        assert!(!wants_texture(&test_cfg(false), "page.jpg"));
    }

    #[test]
    fn 直方图_分箱与预乘还原() {
        // 4 像素：不透明红、不透明绿、半透明白（预乘存储）、全透明
        let img = egui::ColorImage::from_rgba_unmultiplied(
            [2, 2],
            &[
                255, 0, 0, 255, // 红
                0, 255, 0, 255, // 绿
                128, 128, 128, 128, // 半透明白（存储为预乘 64,64,64）
                0, 0, 0, 0, // 全透明
            ],
        );
        let h = Histogram::compute(&img);
        assert_eq!(h.ch[0][255], 1, "红像素 R=255");
        assert_eq!(h.ch[1][255], 1, "绿像素 G=255");
        // 半透明白去预乘还原 ≈128（预乘 64.25→64 取整有 ±1 误差）：
        // 等灰图亮度 = 灰度本身（299+587+114=1000）
        let luma_sum: u32 = h.ch[3][120..=140].iter().sum();
        assert_eq!(luma_sum, 1, "半透明白还原后亮度落 120..140 窗口");
        // 每通道计满全部 4 像素（含全透明——按存储值 0 计入）
        for c in &h.ch {
            assert_eq!(c.iter().sum::<u32>(), 4, "每通道计满 4 像素");
        }
        // bin0 聚合零分量：红像素 G/B + 绿像素 R/B + 透明像素 RGB → B 通道 3 个
        assert_eq!(h.max_bin(), 3);
    }

    #[test]
    fn 直方图_空图防除零() {
        let img = egui::ColorImage::new([4, 4], vec![egui::Color32::BLACK; 16]);
        let h = Histogram::compute(&img);
        // 全黑图：所有像素落 bin 0，max_bin 取 bin0 计数
        assert_eq!(h.max_bin(), 16);
        let empty = egui::ColorImage::new([0, 0], vec![]);
        assert_eq!(Histogram::compute(&empty).max_bin(), 1, "空图兜底 1 防除零");
    }

    #[test]
    fn fit_scale_按纵横比装进视口() {
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 500.0));
        // 2000×500 → 宽度受限：0.5
        assert!((fit_scale(vp, egui::vec2(2000.0, 500.0)) - 0.5).abs() < 1e-6);
        // 200×1000 → 高度受限：0.5
        assert!((fit_scale(vp, egui::vec2(200.0, 1000.0)) - 0.5).abs() < 1e-6);
        // 小图允许放大适配
        assert!((fit_scale(vp, egui::vec2(100.0, 50.0)) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn zoom_pan_键盘缩放_锚点视口中心() {
        // pan=0 时中心锚点缩放保持 pan=0
        assert_eq!(
            zoom_pan(egui::Vec2::ZERO, 2.0, 4.0, egui::Vec2::ZERO),
            egui::Vec2::ZERO
        );
        // 锚点=中心（rel=0）：pan 按倍率比缩放
        let out = zoom_pan(egui::vec2(30.0, -10.0), 2.0, 4.0, egui::Vec2::ZERO);
        assert!((out.x - 60.0).abs() < 1e-4 && (out.y + 20.0).abs() < 1e-4);
    }

    #[test]
    fn zoom_pan_鼠标锚点_图像点保持不动() {
        // pan=(100,0)，缩放 2→4，锚点在右侧 200px 处
        let out = zoom_pan(egui::vec2(100.0, 0.0), 2.0, 4.0, egui::vec2(200.0, 0.0));
        // 锚点下的图像坐标：(200-100)/2 = 50px；新 pan = 200 - 50*4 = 0
        assert!((out.x).abs() < 1e-4 && out.y.abs() < 1e-4);
    }

    #[test]
    fn clamp_pan_大图不露边_小图居中() {
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        // 大幅拖拽 4000×3000 图：边缘恰好贴住视口右/下（不露边）
        let p = clamp_pan(
            egui::vec2(10_000.0, -10_000.0),
            egui::vec2(4000.0, 3000.0),
            vp,
        );
        // x: 中心 clamp 到 [800-2000, 0+2000]=[-1200,2000] → 2000-400=1600
        // y: 中心 clamp 到 [600-1500, 0+1500]=[-900,1500] → -900-300=-1200
        assert_eq!(p, egui::vec2(1600.0, -1200.0));
        // 宽图高不足（disp.y < 视口高）：y 轴拖拽后归零居中，x 轴正常平移
        let p = clamp_pan(egui::vec2(-500.0, 77.0), egui::vec2(4000.0, 300.0), vp);
        assert_eq!(p, egui::vec2(-500.0, 0.0));
        // fit 小图（两轴均 ≤ 视口）：拖拽自动回位居中
        let p = clamp_pan(egui::vec2(50.0, -30.0), egui::vec2(400.0, 300.0), vp);
        assert_eq!(p, egui::Vec2::ZERO);
    }

    #[test]
    #[ignore]
    fn fir_诊断_指令集与耗时() {
        // 临时诊断：dev 下 fir_resize 吞吐异常（~4s/40MP，AVX2 基准应为
        // ~0.1s/40MP）——打印实际选中的指令集 + 分档计时 + 强制各指令集
        // 对比，定位 SIMD 是否真正生效。cargo test fir_诊断 -- --ignored --nocapture
        use fast_image_resize as fir;
        use std::time::Instant;
        println!("fir 默认指令集: {:?}", fir::Resizer::new().cpu_extensions());
        for (nw, nh) in [(3840, 2160u32), (7680, 4320), (11520, 6480)] {
            let img = image::DynamicImage::ImageRgba8(image::RgbaImage::new(2560, 1440));
            let t = Instant::now();
            let out = fir_resize(img, nw, nh);
            println!(
                "fir_resize 2560x1440 → {}x{} ({}MP): {}ms",
                nw,
                nh,
                nw as u64 * nh as u64 / 1_000_000,
                t.elapsed().as_millis()
            );
            std::hint::black_box(&out);
        }
        // 强制指令集对比（11520×6480 档）
        let src = fir::images::Image::from_vec_u8(
            2560,
            1440,
            vec![0u8; 2560 * 1440 * 4],
            fir::PixelType::U8x4,
        )
        .unwrap();
        for ext in [
            fir::CpuExtensions::None,
            fir::CpuExtensions::Sse4_1,
            fir::CpuExtensions::Avx2,
        ] {
            if !ext.is_supported() {
                println!("{ext:?}: CPU 不支持，跳过");
                continue;
            }
            let mut dst = fir::images::Image::new(11520, 6480, fir::PixelType::U8x4);
            let mut resizer = fir::Resizer::new();
            unsafe { resizer.set_cpu_extensions(ext) };
            let opts = fir::ResizeOptions::new()
                .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3));
            let t = Instant::now();
            resizer.resize(&src, &mut dst, &opts).unwrap();
            println!("强制 {ext:?}: {}ms", t.elapsed().as_millis());
            std::hint::black_box(&dst);
        }
    }

    #[test]
    fn resample_基础档按密度预放大() {
        // 阈值型：native×factor ≤ max_dim 才 Lanczos ×factor（factor=ppp，
        // 150% 屏 ×1.5）——翻页即 100% 密度直出
        let out = resample(image::DynamicImage::new_rgb8(20, 10), 4096, false, 2.0);
        assert_eq!((out.width(), out.height()), (40, 20));
        let out = resample(image::DynamicImage::new_rgb8(2000, 1000), 4096, false, 1.5);
        assert_eq!((out.width(), out.height()), (3000, 1500));
        // 超阈值：原尺寸直进（超大图免降采样损失，深放大交给 GPU 双线性）
        let out = resample(image::DynamicImage::new_rgb8(3000, 2000), 4096, false, 1.5);
        assert_eq!(
            (out.width(), out.height()),
            (3000, 2000),
            "×1.5 超 4096 原图直进"
        );
        // ppp=1.0 屏（factor=1）：不无谓重采样
        let out = resample(image::DynamicImage::new_rgb8(800, 600), 4096, false, 1.0);
        assert_eq!((out.width(), out.height()), (800, 600));
    }

    #[test]
    fn resample_高清档clamp放大() {
        // clamp 型：无条件 ×factor、按最长边等比封顶 FULL_MAX_DIM(12288)
        // （factor=ppp×3；native ≤ image_max_dim 资格由触发方保证）
        let out = resample(image::DynamicImage::new_rgb8(1920, 1080), 8192, true, 2.0);
        assert_eq!((out.width(), out.height()), (3840, 2160));
        let out = resample(image::DynamicImage::new_rgb8(4096, 3072), 8192, true, 2.0);
        assert_eq!((out.width(), out.height()), (8192, 6144), "×2 未触顶");
        let out = resample(image::DynamicImage::new_rgb8(2000, 1500), 8192, true, 4.0);
        assert_eq!(
            (out.width(), out.height()),
            (8000, 6000),
            "×4 未触顶整体放大"
        );
        let out = resample(image::DynamicImage::new_rgb8(4096, 1000), 8192, true, 4.0);
        // 最长边 16384→12288 等比缩（scale=3），短边随比例 1000→3000
        //（逐边独立 clamp 会畸变成 12288×4000）
        assert_eq!((out.width(), out.height()), (12288, 3000));
        // 像素预算二次封顶：正方形极端图 12288²≈151MP 超预算等比缩回
        let out = resample(image::DynamicImage::new_rgb8(4096, 4096), 4096, true, 3.0);
        assert_eq!(out.width(), out.height(), "正方形极端图保持等比");
        assert!(
            out.width() as u64 * out.height() as u64 <= FULL_MAX_PIXELS,
            "总像素不超预算"
        );
    }

    #[test]
    fn resample_病态图8192兜底() {
        // 基础档兜底：>8192 病图等比降到 8192（快速 box 采样 + mipmap）
        let out = resample(image::DynamicImage::new_rgb8(9000, 7000), 4096, false, 1.0);
        assert_eq!((out.width(), out.height()), (8192, 6372)); // 7000×8192/9000=6371.6→6372
                                                               // 高清档：最长边封顶 12288 + 像素预算兜底（防御性路径，full 资格
                                                               // 图理论到不了这）
        let out = resample(image::DynamicImage::new_rgb8(10000, 8000), 8192, true, 3.0);
        assert!(out.width() <= FULL_MAX_DIM && out.height() <= FULL_MAX_DIM);
        assert!(
            out.width() as u64 * out.height() as u64 <= FULL_MAX_PIXELS,
            "总像素不超预算"
        );
        assert!(
            (out.width() as f64 / out.height() as f64 - 1.25).abs() < 0.01,
            "病态图降采样仍保持 10000:8000 比例"
        );
    }

    #[test]
    fn thumbnail_cap_动图降采样() {
        // 动图帧：不预放大，超 cap 降采样（全帧驻显存）；限内原样
        let out = thumbnail_cap(image::DynamicImage::new_rgb8(6000, 4000), 4096);
        assert_eq!((out.width(), out.height()), (4096, 2731)); // 4000×4096/6000=2730.7
        let out = thumbnail_cap(image::DynamicImage::new_rgb8(100, 80), 4096);
        assert_eq!((out.width(), out.height()), (100, 80));
    }

    #[test]
    fn sharpen_full_锐化行为() {
        // 纯色图：无边缘无细节，锐化不动（px 与 blur(px) 全等）
        let flat = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
            16,
            16,
            image::Luma([128]),
        ));
        let out = sharpen_full(flat, &sharpen_on(0.6)).0.to_rgba8();
        assert!(out.pixels().all(|p| p[0] == 128));
        // 垂直阶跃：左 64 右 192（取中灰区间防 0/255 饱和掩盖效果），
        // 边缘内侧低侧压暗 = 坡度变陡；高侧在 flank 带抑制区内（dilate=1
        // 合并夹缝覆盖边界），正向增强被压 = 白边抑制生效、无过冲
        let mut img = image::GrayImage::new(16, 8);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 8 { 64 } else { 192 }]);
            }
        }
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img), &sharpen_on(0.6))
            .0
            .to_rgba8();
        assert!(out.get_pixel(7, 4)[0] < 64, "边缘低侧应压暗");
        assert!(
            out.get_pixel(8, 4)[0] <= 195,
            "边缘高侧过冲应被压制（默认曲线末点保留 5% 锐感，残余 ≤2 级）"
        );
    }

    #[test]
    fn sharpen_full_白边抑制() {
        // 垂直阶跃（128 级差）：同一边缘两侧 blur 对称、差值幅度相同——
        // shoot 白边抑制只压正向增强（亮侧），dilate=1 合并 flank 夹缝后
        // 亮侧边界像素落在抑制区内：过冲全额压灭（白边消失），暗侧压暗
        // （负向增强不受 ratio）保留 = 边缘对比仍增强
        let mut img = image::GrayImage::new(16, 8);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 8 { 64 } else { 192 }]);
            }
        }
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img), &sharpen_on(0.6))
            .0
            .to_rgba8();
        let hi = i32::from(out.get_pixel(8, 4)[0]) - 192;
        let lo = 64 - i32::from(out.get_pixel(7, 4)[0]);
        assert!(
            hi <= 2,
            "亮侧过冲应被压制（默认曲线末点 0.05 残余 ≤2 级）: +{hi}"
        );
        assert!(lo > 0, "暗侧压暗应保留（锐感来源）: −{lo}");
        // curve = []（None）关闭：恢复对称 unsharp，两侧幅度一致（±1 给取整）
        let mut img = image::GrayImage::new(16, 8);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 8 { 64 } else { 192 }]);
            }
        }
        let mut cfg = sharpen_on(0.6);
        cfg.white_edge = None;
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img), &cfg)
            .0
            .to_rgba8();
        let hi = i32::from(out.get_pixel(8, 4)[0]) - 192;
        let lo = 64 - i32::from(out.get_pixel(7, 4)[0]);
        assert!(
            (hi - lo).abs() <= 1,
            "关闭 shoot 后两侧应恢复对称: +{hi}/−{lo}"
        );
        // 细纹理不受抑制：幅度 8 的单像素凸起 shoot≈0（各尺度方差相近），
        // 正向增强全额保留
        let mut img = image::GrayImage::from_pixel(16, 16, image::Luma([128]));
        *img.get_pixel_mut(8, 8) = image::Luma([136]);
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img), &sharpen_on(0.6))
            .0
            .to_rgba8();
        let up = i32::from(out.get_pixel(8, 8)[0]) - 128;
        assert!(up >= 3, "细纹理锐化被 shoot 误伤: +{up}");
    }

    #[test]
    fn shoot_敏感度增益展宽缓坡() {
        // 方向保留（不取 abs）语义：缓坡（4 级/px，v7−v3≈3.33·slope²≈53）
        // 方差差量级小，gain=1 落在 0..255 低段够不到曲线拐点；高 gain 线性
        // 展宽到 255（白边抑制生效区）。宽边阶跃 flank（差值大）不受增益影响
        let mut img = image::GrayImage::new(32, 16);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([64 + (4 * x as u32).min(128) as u8]);
            }
        }
        let lut = build_white_edge_lut(&crate::config::DEFAULT_WHITE_EDGE_CURVE);
        let (_, lo) = shoot_ratios(&img, &lut, 1.0, 1, true);
        let (_, hi) = shoot_ratios(&img, &lut, 16.0, 1, true);
        let (lo, hi) = (lo.unwrap(), hi.unwrap());
        let (lo, hi): (&[u8], &[u8]) = (&lo, &hi);
        let (m, e) = (4 * 32 + 16, 4 * 32 + 20);
        assert!(lo[m] < 255, "增益前缓坡方差差不应顶满: {}", lo[m]);
        assert_eq!(hi[m], 255, "高增益应把缓坡方差差展宽到满");
        assert_eq!(hi[e], 255, "缓坡中部全域（7×7 完整窗口内）同样展宽");
        // 阶跃强边 flank 在 gain=1 就已拉满（增益只影响低段）
        let mut img = image::GrayImage::new(16, 8);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 8 { 64 } else { 192 }]);
            }
        }
        let (_, m) = shoot_ratios(&img, &lut, 1.0, 1, true);
        let m: &[u8] = &m.unwrap();
        assert_eq!(m[4 * 16 + 9], 255, "宽边 flank 在低增益下即应拉满");
        // flank 带外扩：半径 1 把边缘外侧 1~2px 的高亮带向外扩，覆盖真边缘
        // 线本身（膨胀前边缘正中 v3≈v7 差值收敛、落在低段）；0 = 关时可见偏移
        let (_, d0) = shoot_ratios(&img, &lut, 1.0, 0, true);
        let (_, d1) = shoot_ratios(&img, &lut, 1.0, 1, true);
        let (d0, d1) = (d0.unwrap(), d1.unwrap());
        let (d0, d1): (&[u8], &[u8]) = (&d0, &d1);
        assert!(
            d0[4 * 16 + 8] < 255,
            "外扩前边缘正中是低值（flank 偏外 1~2px）"
        );
        assert_eq!(d1[4 * 16 + 8], 255, "外扩后 flank 带覆盖边缘正中");
        // 方向保留的已知取舍（用户裁定）：细于窗口的结构（2px 亮线）方差差
        // 方向反转（v5>v3>v7），clamp 负值为 0——线上与两侧 flank 全部无响应，
        // 靠宽边 flank 带与外扩覆盖；abs 方案实测毛躁紊乱已弃用
        let mut img = image::GrayImage::from_pixel(16, 16, image::Luma([64]));
        for y in 0..16u32 {
            img.put_pixel(8, y, image::Luma([192]));
            img.put_pixel(9, y, image::Luma([192]));
        }
        let (_, m) = shoot_ratios(&img, &lut, 16.0, 0, true);
        let m: &[u8] = &m.unwrap();
        assert_eq!(m[4 * 16 + 8], 0, "2px 细线在方向保留语义下无响应（线上）");
        assert_eq!(m[4 * 16 + 10], 0, "2px 细线在方向保留语义下无响应（flank）");
    }

    #[test]
    fn 白边曲线_lut_构建() {
        // 折点精确落值 + 分段线性插值 + 隐含首点 [0,1] + 末点平坦延伸
        let lut = build_white_edge_lut(&[(100.0, 0.8), (200.0, 0.6), (240.0, 0.2), (255.0, 0.05)]);
        assert_eq!(lut[0], 1.0, "shoot_v=0（细纹理）恒不抑制");
        assert!((lut[50] - 0.9).abs() < 1e-6, "首段插值 0→100 中点应为 0.9");
        assert_eq!(lut[100], 0.8, "折点精确落值");
        assert_eq!(lut[200], 0.6, "折点精确落值");
        assert!((lut[220] - 0.4).abs() < 1e-6, "200→240 段中点应为 0.4");
        assert_eq!(lut[240], 0.2, "折点精确落值");
        assert_eq!(lut[255], 0.05, "末点落值");
        // 解析：空表 = 关闭；非法折点回退默认曲线（x 重复不递增 / x 越界 /
        // y 越界三种）；合法曲线原样构建
        assert_eq!(parse_white_edge_curve(&[]), (None, true), "空数组 = 关闭");
        let (lut, ok) = parse_white_edge_curve(&[(100.0, 0.8), (100.0, 0.5)]);
        let Some(lut) = lut else {
            panic!("非法应回退默认曲线而非关闭")
        };
        assert!(!ok && lut[100] != 0.5, "x 不递增应回退默认曲线");
        let (lut, ok) = parse_white_edge_curve(&[(300.0, 0.8)]);
        let Some(lut) = lut else {
            panic!("非法应回退默认曲线而非关闭")
        };
        assert!(!ok && lut[255] != 0.8, "x 越界应回退默认曲线");
        let (lut, ok) = parse_white_edge_curve(&[(100.0, 1.5)]);
        let Some(lut) = lut else {
            panic!("非法应回退默认曲线而非关闭")
        };
        assert!(!ok && lut[100] != 1.5, "y 越界应回退默认曲线");
        let default = build_white_edge_lut(&crate::config::DEFAULT_WHITE_EDGE_CURVE);
        assert_eq!(lut, default, "回退值应等于默认曲线 LUT");
        let (lut, ok) = parse_white_edge_curve(&[(100.0, 0.8)]);
        let Some(lut) = lut else {
            panic!("合法曲线应为 Some")
        };
        assert!(ok && lut[100] == 0.8, "合法折点原样构建");
    }

    #[test]
    fn shoot_黑方块方向对称性诊断() {
        // 白底黑方块四条边的 shoot 响应必须旋转对称——方差与窗口定址都是
        // 方向无关的，任何方向差异都是实现缺陷。用户在真实照片 dump 上观察
        // 到的"上/右 255、左/下 160"类不对称来自背景侧差异（v3 吃掉方差差，
        // 见架构 13.4），合成均匀图上不存在
        let lut = build_white_edge_lut(&crate::config::DEFAULT_WHITE_EDGE_CURVE);
        for (name, sq) in [("黑0", 0u8), ("黑128", 128u8), ("黑192", 192u8)] {
            let mut img = image::GrayImage::from_pixel(32, 32, image::Luma([255]));
            for y in 11..21u32 {
                for x in 11..21u32 {
                    img.put_pixel(x, y, image::Luma([sq]));
                }
            }
            for dilate in [0u32, 1] {
                let (_, m) = shoot_ratios(&img, &lut, 1.0, dilate, true);
                let raw: Vec<u8> = m.unwrap().into_raw();
                // 旋转 90°（(x,y)→(31−y,x) 映射回同尺寸）逐点比对
                for y in 0..32usize {
                    for x in 0..32usize {
                        let rotated = raw[x * 32 + (31 - y)];
                        assert_eq!(
                            raw[y * 32 + x],
                            rotated,
                            "{name} dilate={dilate} ({x},{y}) 旋转后不一致"
                        );
                    }
                }
                if dilate == 0 {
                    println!("== {name} 方块 sv 网格（7..25）==");
                    for y in 7..25usize {
                        let row: String = (7..25usize)
                            .map(|x| format!("{:4}", raw[y * 32 + x]))
                            .collect();
                        println!("{row}");
                    }
                }
            }
        }
    }

    #[test]
    fn decode_锐化接线_解码链生效() {
        // 垂直阶跃（左 64 右 192）同图开/关锐化各解一次，full 档下边缘
        // 像素必须有差异——守卫"sharpen 参数传了但 decode_any 没用"的
        // 接线回归（曾因编辑竞态断线且既有测试全绿，靠编译警告才暴露）
        let mut img = image::GrayImage::new(64, 32);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 32 { 64 } else { 192 }]);
            }
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sharpen_wire.png");
        std::fs::write(&p, buf.get_ref()).unwrap();
        let decode = |cfg: SharpenCfg| {
            let out = decode_image(
                &ImageSource::File(p.clone()),
                8192,
                true,
                1.0,
                "s.png",
                &cfg,
                0,
            )
            .unwrap();
            let (out, _) = out;
            let Decoded::Static(img) = out else {
                panic!("PNG 应走静态路径");
            };
            img
        };
        let (off, on) = (decode(SharpenCfg::OFF), decode(sharpen_on(0.6)));
        std::fs::remove_file(&p).ok();
        assert_eq!(off.size, on.size);
        assert_ne!(off.pixels, on.pixels, "锐化开/关边缘像素应有差异");
    }

    #[test]
    fn 降噪独立于锐化开关() {
        // 锐化关（on=false）而降噪开时，降噪仍生效——两开关解耦（历史上
        // 降噪嵌在 sharpen_full 里被 on 连带关闭，是用户报的 bug）
        let cfg = SharpenCfg {
            on: false,
            denoise: true,
            algo: DenoiseAlgo::Guided,
            guided_strength: 50.0,
            guided_radius: 2,
            ..sharpen_on(0.0)
        };
        // 带噪平坦图：随机点噪声被导向滤波压掉
        let mut img = image::RgbaImage::new(64, 64);
        for y in 0..64u32 {
            for x in 0..64u32 {
                let n = ((x * 7 + y * 13) % 11) as i32 - 5; // 伪随机 ±5
                let v = (128 + n).clamp(0, 255) as u8;
                img.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        let before = img.clone();
        let after = sharpen_full(image::DynamicImage::ImageRgba8(img), &cfg)
            .0
            .to_rgba8();
        assert_ne!(before.as_raw(), after.as_raw(), "锐化关时降噪仍应生效");
        // 关闭档（全关）零开销直通
        let (same, mask) = sharpen_full(
            image::DynamicImage::ImageRgba8(before.clone()),
            &SharpenCfg::OFF,
        );
        assert_eq!(same.to_rgba8().as_raw(), before.as_raw(), "全关应原样返回");
        assert!(mask.is_none(), "全关不应产出 shoot 掩码");
    }

    /// 内存中生成 PNG 字节
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// 内存中生成 2 帧 GIF（默认帧延迟 0 → 触发 100ms 钳正）
    fn gif_bytes_2帧(w: u32, h: u32) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut buf);
            for _ in 0..2 {
                let fr = image::Frame::new(image::RgbaImage::from_pixel(
                    w,
                    h,
                    image::Rgba([255u8, 0, 0, 255]),
                ));
                enc.encode_frame(fr).unwrap();
            }
        } // drop enc 释放 &mut buf
        buf.into_inner()
    }

    #[test]
    fn anim_delay_短帧钳正() {
        use std::time::Duration;
        // from_numer_denom_ms(n, de) = n/de 毫秒（比值即毫秒，image animation.rs）
        let d = image::Delay::from_numer_denom_ms(100, 1000); // 0.1ms
        assert_eq!(anim_delay(d), Duration::from_millis(100));
        let d = image::Delay::from_numer_denom_ms(0, 1000); // 0ms
        assert_eq!(anim_delay(d), Duration::from_millis(100), "0 延迟钳 100ms");
        let d = image::Delay::from_numer_denom_ms(1, 100); // 0.01ms
        assert_eq!(anim_delay(d), Duration::from_millis(100), "亚毫秒钳 100ms");
        let d = image::Delay::from_numer_denom_ms(50, 1); // 50ms（GIF 5cs 真实值）
        assert_eq!(anim_delay(d), Duration::from_millis(50), "50ms 保持");
        let d = image::Delay::from_numer_denom_ms(80, 1); // 80ms（GIF 8cs）
        assert_eq!(anim_delay(d), Duration::from_millis(80), "80ms 保持");
    }

    #[test]
    fn decode_动画多帧与延迟钳正() {
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim.gif");
        std::fs::write(&p, gif_bytes_2帧(8, 6)).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            4096,
            false,
            2.0,
            "t.png",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        let Decoded::Anim(frames) = out else {
            panic!("2 帧 GIF 应解出 Anim");
        };
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0.size, [8, 6]);
        // GIF 帧延迟 0 → 浏览器惯例钳 100ms
        assert_eq!(frames[0].1, Duration::from_millis(100));
    }

    /// 生成带真实帧延迟的多帧 GIF（Frame::from_parts 显式指定 Delay；
    /// delay_ms 须为 10 的倍数——GIF 磁盘格式以 10ms 为单位存储）
    fn gif_bytes_delayed(w: u32, h: u32, delay_ms: u32, n: u32) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut buf);
            for i in 0..n {
                let fr = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(
                        w,
                        h,
                        image::Rgba([if i % 2 == 0 { 255 } else { 0 }, 0, 0, 255]),
                    ),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(delay_ms, 1),
                );
                enc.encode_frame(fr).unwrap();
            }
        } // drop enc 释放 &mut buf
        buf.into_inner()
    }

    #[test]
    fn decode_动画真实延迟() {
        // 回归：旧版 anim_delay 把毫秒比值再 ×1000，80ms 的 GIF 帧被算成
        // 80s/帧 → 画面永远停在第一帧（UI 只显示首帧）。image 0.25 的 GIF
        // 解码产出 Delay = (delay_cs*10, 1)，即比值已换算成毫秒，不得再乘
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim_delay.gif");
        std::fs::write(&p, gif_bytes_delayed(8, 6, 80, 2)).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            4096,
            false,
            2.0,
            "t.png",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        let Decoded::Anim(frames) = out else {
            panic!("带真实延迟的 2 帧 GIF 应解出 Anim");
        };
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].1,
            Duration::from_millis(80),
            "80ms 帧延迟应按原值播放（旧版 ×1000 bug 会算成 80s）"
        );
    }

    #[test]
    fn decode_单帧gif归静态() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut buf);
            enc.encode_frame(image::Frame::new(image::RgbaImage::new(4, 4)))
                .unwrap();
        } // drop enc 释放 &mut buf
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("single.gif");
        std::fs::write(&p, buf.into_inner()).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            4096,
            false,
            2.0,
            "t.png",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        assert!(
            matches!(out, Decoded::Static(_)),
            "单帧动画应归静态（免时钟开销）"
        );
    }

    #[test]
    fn decode_静态webp归静态() {
        // 回归：image-webp 静态文件 num_frames()==0，若走 into_frames 解出
        // 空集误报"动画无有效帧"回退 mpv（LOG2 实测踩坑）；webp 必须走静图
        let img = image::DynamicImage::new_rgb8(8, 6);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::WebP).unwrap();
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.webp");
        std::fs::write(&p, buf.into_inner()).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            4096,
            false,
            2.0,
            "t.png",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        let Decoded::Static(tex) = out else {
            panic!("静态 webp 应走静图路径");
        };
        // 8x6 ≤512 触发小图 ×2 上采样（resample 既有行为）
        assert_eq!(tex.size, [16, 12]);
    }

    #[test]
    fn decode_动画webp逐帧() {
        // 夹具：ffmpeg libwebp 生成的 2 帧动画（红/蓝 20×14，100ms/帧，
        // VP8X ANIM 位置位 + ANIM/ANMF 块）——has_animation 路由进 gif
        // 同款逐帧机制；动图不预放大，帧为画布原尺寸。
        // 夹具由 cargo run --bin smoke_setup 生成于 smoke/img/anim2.webp
        // （src/bin/smoke_setup.rs::gen_img 同参数管线），缺失即 panic
        let bytes = std::fs::read(crate::smoke::require("img/anim2.webp")).unwrap();
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim2.webp");
        std::fs::write(&p, bytes).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            4096,
            false,
            2.0,
            "anim2.webp",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        let Decoded::Anim(frames) = out else {
            panic!("动画 webp 应走逐帧路径（Decoded::Anim）");
        };
        assert_eq!(frames.len(), 2, "两个 ANMF 帧都应解出");
        assert_eq!(frames[0].0.size, [20, 14], "动图帧为画布原尺寸（不预放大）");
        assert_eq!(frames[0].1, Duration::from_millis(100));
        // 帧序与颜色：首帧红、次帧蓝（lavfi 纯色 + VP8L 无损，通道优势明显）
        let (r, g, b, _) = frames[0].0.pixels[0].to_tuple();
        assert!(
            r > 200 && g < 60 && b < 60,
            "首帧应为红色，实为 ({r},{g},{b})"
        );
        let (r, g, b, _) = frames[1].0.pixels[0].to_tuple();
        assert!(
            b > 200 && r < 60 && g < 60,
            "次帧应为蓝色，实为 ({r},{g},{b})"
        );
    }

    #[test]
    fn decode_高清路径中图两倍预放大() {
        // 800×600：基础档阈值内升 ×factor（见 decode_file_从临时文件解码），
        // 高清档 clamp 型同样 ×2（800×2=1600 ≤12288 未触顶）——深放大
        // 区间 Lanczos 细节优于 GPU 双线性
        let png = png_bytes(800, 600);
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("full.png");
        std::fs::write(&p, &png).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            8192,
            true,
            2.0,
            "t.png",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        let Decoded::Static(img) = out else {
            panic!("PNG 应走静态路径");
        };
        assert_eq!(img.size, [1600, 1200]);
    }

    #[test]
    fn decode_高清路径按屏幕密度预放大() {
        // 800×600 ×1.5（150% DPI 屏 100% 查看密度）→ 1200×900：恰好 1:1
        // 物理像素、单级 Lanczos 直出（固定 ×2 再被 mip 缩回是两级重采样，
        // 锐边被抹——用户实测 100% 查看发糊的根因）
        let png = png_bytes(800, 600);
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("full_dpi.png");
        std::fs::write(&p, &png).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            8192,
            true,
            1.5,
            "t.png",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        let Decoded::Static(img) = out else {
            panic!("PNG 应走静态路径");
        };
        assert_eq!(img.size, [1200, 900]);
    }

    #[test]
    fn decode_file_从临时文件解码() {
        // 600×400：大于上采样阈值（512）且小于降采样阈值，尺寸原样保留
        let png = png_bytes(600, 400);
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.png");
        std::fs::write(&p, &png).unwrap();
        let out = decode_image(
            &ImageSource::File(p.clone()),
            4096,
            false,
            2.0,
            "t.png",
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        std::fs::remove_file(&p).ok();
        let (out, _) = out;
        let Decoded::Static(img) = out else {
            panic!("PNG 应走静态路径");
        };
        assert_eq!(img.size, [1200, 800]);
    }

    /// 直接塞一帧进服务缓存（跳过后台线程，同步可控）
    fn feed(svc: &mut ImageService, key: ImgKey, decoded: Decoded, name: &str) {
        let hist = match &decoded {
            Decoded::Static(img) => Some(Histogram::compute(img)),
            Decoded::Anim(a) => a.first().map(|(f, _)| Histogram::compute(f)),
        };
        // 测试直喂：代次机制要求 Ready 与 pending 占位匹配——pending 无该
        // key（未走真实 request）时补一个占位，有则沿用真实代次
        let gen = match svc.pending.get(&key) {
            Some((_, _, g)) => *g,
            None => {
                svc.base_gen += 1;
                let g = svc.base_gen;
                svc.pending
                    .insert(key, (std::time::Instant::now(), IoClass::Decode, g));
                g
            }
        };
        let _ = svc.tx.send(ImgMsg::Ready {
            key,
            decoded,
            hist,
            native: None,
            name: name.to_owned(),
            orient: 1,
            class: IoClass::Decode,
            gen,
        });
        svc.poll();
    }

    #[test]
    fn lru_容量逐出_跳过当前() {
        let mut svc = ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        let cur = (1, 5);
        let ready = |svc: &mut ImageService, key: ImgKey| {
            feed(
                svc,
                key,
                Decoded::Static(egui::ColorImage::new(
                    [4, 4],
                    vec![egui::Color32::WHITE; 16],
                )),
                "t.png",
            );
        };
        // 依次就绪 3 张：cap=2 应逐出最早的 (1,0)，保留 (1,1) 与当前 (1,5)
        ready(&mut svc, (1, 0));
        ready(&mut svc, (1, 1));
        ready(&mut svc, cur);
        assert!(svc.frame((1, 1)).is_some());
        assert!(svc.frame(cur).is_some());
        assert!(svc.frame((1, 0)).is_none(), "最久未用应被逐出");
        // 访问顺序变为 [(1,5), (1,1)]；新入 (1,2) 超容时跳过当前 (1,5)，逐出 (1,1)
        let _ = svc.frame(cur); // cur 提到队首
        ready(&mut svc, (1, 2));
        assert!(svc.frame((1, 2)).is_some());
        assert!(svc.frame(cur).is_some(), "当前显示的图不应被逐出");
        assert!(svc.frame((1, 1)).is_none());
    }

    #[test]
    fn lru_预取洪水不逐出正在显示的条目() {
        let mut svc = ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        let cur = (1, 5);
        let ready = |svc: &mut ImageService, key: ImgKey| {
            feed(
                svc,
                key,
                Decoded::Static(egui::ColorImage::new(
                    [4, 4],
                    vec![egui::Color32::WHITE; 16],
                )),
                "t.png",
            );
        };
        // 显示中（frame 标记 shown）
        ready(&mut svc, cur);
        assert!(svc.frame(cur).is_some());
        // 模拟滚轮连翻：±1 预取陆续就绪推入，超容时不得挤掉显示中的 cur
        ready(&mut svc, (1, 4));
        ready(&mut svc, (1, 6));
        ready(&mut svc, (1, 3));
        // 卡死 bug 回归：修复前 cur 被逐出且 pending 已空，frame=None 永久占位
        assert!(svc.frame(cur).is_some(), "正在显示的图不应被预取洪水挤出");
    }

    #[test]
    fn frame_动图推进状态机() {
        let mut svc = ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        let key = (2, 0);
        let frame = |c: u8| egui::ColorImage::new([4, 4], vec![egui::Color32::from_gray(c); 16]);
        feed(
            &mut svc,
            key,
            Decoded::Anim(vec![
                (frame(10), Duration::from_millis(30)),
                (frame(20), Duration::from_millis(30)),
                (frame(30), Duration::from_millis(30)),
            ]),
            "t.gif",
        );
        // 首次取帧：第 0 帧，给出起播时长
        let (tex, rest) = svc.frame(key).expect("命中");
        assert_eq!(tex.name(), "img-2-0-f0");
        let rest = rest.expect("动图应有切帧时长");
        assert!(rest <= Duration::from_millis(30));
        // 未到点重复取：仍第 0 帧
        std::thread::sleep(Duration::from_millis(5));
        let (tex, _) = svc.frame(key).unwrap();
        assert_eq!(tex.name(), "img-2-0-f0");
        // 睡过延迟：推进到第 1 帧
        std::thread::sleep(rest + Duration::from_millis(5));
        let (tex, _) = svc.frame(key).unwrap();
        assert_eq!(tex.name(), "img-2-0-f1");
        // 环绕：再推 2 次回到 f0
        for _ in 0..2 {
            let (_, rest) = svc.frame(key).unwrap();
            std::thread::sleep(rest.unwrap() + Duration::from_millis(5));
            let _ = svc.frame(key).unwrap(); // 推进时钟
        }
        let (tex, _) = svc.frame(key).unwrap();
        assert_eq!(tex.name(), "img-2-0-f0", "应环绕回第 0 帧");
    }

    #[test]
    fn frame_静态图无时钟() {
        let mut svc = ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        let key = (3, 0);
        feed(
            &mut svc,
            key,
            Decoded::Static(egui::ColorImage::new(
                [4, 4],
                vec![egui::Color32::WHITE; 16],
            )),
            "t.png",
        );
        let (tex, rest) = svc.frame(key).unwrap();
        assert_eq!(tex.name(), "img-3-0-f0");
        assert!(rest.is_none(), "静态图不应驱动 repaint");
    }

    #[test]
    fn request_去重与失败事件() {
        let mut svc = ImageService::new(egui::Context::default(), 3, Default::default(), 0, test_sched());
        let missing = || ImageSource::File(std::path::PathBuf::from("definitely_missing.png"));
        // 同 key 重复请求：第一次发牌解码，第二次被 pending 幂等检查吞掉
        //（在途去重优先于 admission，不重复排队）
        svc.request((9, 9), missing(), 4096, "t.png", IoClass::Decode);
        svc.request((9, 9), missing(), 4096, "t.png", IoClass::Decode);
        assert_eq!(svc.pending.len(), 1);
        // 缺失文件 → Failed 事件，pending 清除（自旋等待后台线程，毫秒级）
        let evs = loop {
            let evs = svc.poll();
            if !evs.is_empty() {
                break evs;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        match &evs[0] {
            ImgEvent::Failed(key, _) => assert_eq!(*key, (9, 9)),
            ImgEvent::Ready(_) => panic!("缺失文件不应解码成功"),
        }
        assert!(svc.pending.is_empty());
        assert!(svc.deferred.is_empty(), "在途期重复请求不入队，失败后无遗留");
        // pending 清除后可重新请求（重试语义）
        svc.request(
            (9, 9),
            ImageSource::File(std::path::PathBuf::from("x.png")),
            4096,
            "t.png",
            IoClass::Decode,
        );
        assert_eq!(svc.pending.len(), 1);
    }

    /// full 零 IO 门控（优先级占槽模型的记账对齐）：原图缓存未命中 = 真源
    /// 读，按 Decode 发牌占槽（FullFailed 回账）；**命中 = 纯 CPU 重采样
    /// （fast_image_resize + 锐化）零 IO，不占槽直接开工**——占着 IO 槽
    /// 排队纯属错门（full 升级大多数情况走缓存命中路径）
    #[test]
    fn full_原图缓存命中不占槽() {
        // orig_budget 给足（16MB）：natives 准入按预算逐出，0 会让测试原图
        // 推入即逐出、命中路径测不到
        let mut svc = ImageService::new(
            egui::Context::default(),
            3,
            Default::default(),
            16 * 1024 * 1024,
            test_sched(),
        );
        let missing = || ImageSource::File(std::path::PathBuf::from("definitely_missing.png"));
        // ① 未命中：真源读占槽
        svc.request_full((9, 9), missing(), "t.png", SharpenCfg::OFF);
        assert!(svc.is_full_pending((9, 9)));
        assert_eq!(svc.sched.decode_out(), 1, "源读 full 应占 Decode 槽");
        // FullFailed{io:true} 回账归零
        let _ = svc.tx.send(ImgMsg::FullFailed {
            key: (9, 9),
            epoch: svc.full_epoch,
            io: true,
        });
        svc.poll();
        assert_eq!(svc.sched.decode_out(), 0, "回账后应归零");
        // ② 命中：原图已入 natives（base worker 产出的常见状态），full
        // 直接开工且零占槽
        // 测试直喂（代次匹配）：补 pending 占位 + 捕获代次
        svc.base_gen += 1;
        let g2 = svc.base_gen;
        svc.pending.insert(
            (9, 8),
            (std::time::Instant::now(), IoClass::Decode, g2),
        );
        let _ = svc.tx.send(ImgMsg::Ready {
            key: (9, 8),
            decoded: Decoded::Static(egui::ColorImage::new(
                [4, 4],
                vec![egui::Color32::WHITE; 16],
            )),
            hist: None,
            native: Some(image::DynamicImage::new_rgba8(8, 8)),
            name: "t2.png".into(),
            orient: 1,
            class: IoClass::Decode,
            gen: g2,
        });
        svc.poll();
        svc.request_full((9, 8), missing(), "t2.png", SharpenCfg::OFF);
        assert!(svc.is_full_pending((9, 8)), "命中 full 应直接开工");
        assert_eq!(
            svc.sched.decode_out(),
            0,
            "原图缓存命中的纯 CPU full 不得占 IO 槽"
        );
        // ③ 迟到收尾：FullReady{io:false} 不回账（计数保持 0，不饱和下溢）
        let _ = svc.tx.send(ImgMsg::FullReady {
            key: (9, 8),
            img: egui::ColorImage::new([4, 4], vec![egui::Color32::WHITE; 16]),
            name: "t2.png".into(),
            epoch: svc.full_epoch,
            io: false,
        });
        svc.poll();
        assert_eq!(svc.sched.decode_out(), 0);
    }

    // ---- 相机 RAW 预览（方案 §4.3）----

    #[test]
    fn is_raw_photo_ext_判定() {
        // Sony ARW 家族 + Nikon NEF 家族，大小写不敏感、可带路径
        assert!(is_raw_photo_ext("DSC01234.ARW"));
        assert!(is_raw_photo_ext("dsc.nef"));
        assert!(is_raw_photo_ext("a/b/c.NRW"));
        assert!(is_raw_photo_ext("x.SRF"));
        assert!(is_raw_photo_ext("x.sr2"));
        assert!(is_raw_photo_ext("x.ArQ"));
        // 非 RAW / 无扩展名裸名（Path::extension 语义）/ 后缀更长不算
        assert!(!is_raw_photo_ext("x.jpg"));
        assert!(!is_raw_photo_ext("arw"));
        assert!(!is_raw_photo_ext("x.arwx"));
    }

    #[test]
    fn decode_raw_preview_垃圾字节报错供fall_through() {
        // 改名伪 raw（内容非相机文件）→ 识别失败 Err → decode_image
        // 拦截分支 fall through 原路径（ImageReader 大概率也失败 → mpv 回退）
        assert!(decode_raw_preview(b"definitely not a camera raw file", None).is_err());
        assert!(decode_raw_preview(&[], None).is_err());
    }

    // EXIF 方向 8 值映射测试已迁移 src/meta.rs（apply_orientation_八个exif方向映射
    // + 与旋转映射自洽），此处不再重复

    #[test]
    fn finish_static_抽取后行为不变() {
        // 共享尾部抽取回归：基础档阈值内 ×factor 预放大；原图资格命中时
        // 顺带带回供 full 升级复用
        let (out, native) = finish_static(
            image::DynamicImage::new_rgb8(100, 50),
            4096,
            false,
            2.0,
            &SharpenCfg::OFF,
            0,
        )
        .unwrap();
        let Decoded::Static(out) = out else {
            panic!("finish_static 恒返回 Static");
        };
        assert_eq!(out.size, [200, 100]);
        assert!(native.is_some(), "100×50×2≤4096 应命中原图保留资格");
    }

    // ---- per-file 显示对比度（W/S 快捷键）----

    #[test]
    fn contrast_lut_方向边界与单调() {
        assert!(contrast_lut(0).is_none(), "n=0 恒等应返 None（调用方零开销直通）");
        let up = contrast_lut(1).unwrap();
        let down = contrast_lut(-1).unwrap();
        // 对比度方向：+1 步（×1.25）亮处更亮暗处更暗（锚点 127.5 不动）；
        // -1 步（×0.8）向中灰收敛
        assert!(up[192] > 192, "+1 步亮半区应更亮，实际 {}", up[192]);
        assert!(up[64] < 64, "+1 步暗半区应更暗，实际 {}", up[64]);
        assert!(down[192] < 192 && down[64] > 64, "-1 步应向中灰收敛");
        // 端点钳制 + 全程单调不减（亮度序保持，直方图形状不折叠）
        assert_eq!(up[0], 0);
        assert_eq!(up[255], 255);
        for w in up.windows(2) {
            assert!(w[0] <= w[1], "LUT 应单调不减: {} > {}", w[0], w[1]);
        }
        // 锚点不动（128 落 127.5+0.5s，任何步数四舍五入回 128——黑位不抬升
        // 是"不发灰"的关键不变量）
        assert_eq!(up[128], 128);
        assert_eq!(down[128], 128);
        // 步进复合：+2 步比 +1 步对比更强；深降档不发散仍单调
        let up2 = contrast_lut(2).unwrap();
        assert!(up2[192] > up[192] && up2[64] < up[64], "+2 步应比 +1 步对比更强");
        let deep = contrast_lut(-6).unwrap();
        // 深降档向锚点收敛：两端都拉回中灰附近（不再顶到黑/白边界）
        assert!(deep[255] < 255 && deep[0] > 0);
        for w in deep.windows(2) {
            assert!(w[0] <= w[1]);
        }
    }

    #[test]
    fn apply_contrast_像素与alpha保持() {
        let mut img = egui::ColorImage::from_rgba_unmultiplied([1, 1], &[200, 200, 200, 255]);
        let lut = contrast_lut(2).unwrap();
        // ColorImage 存储为预乘 alpha（from_rgba_unmultiplied 内部转换），
        // 半透明像素 RGB 已缩放——用不透明像素（alpha=255，预乘=原值，
        // 与真实解码产物一致）做方向性断言，alpha 恒不变
        let before = img.pixels[0].r();
        apply_contrast(&mut img, &lut);
        let px = img.pixels[0];
        assert!(px.r() > before, "亮值（>锚点）经增强 LUT 应更亮");
        assert_eq!(px.a(), 255, "alpha 不动");
    }

    /// 塞一帧就绪消息（native 可带）进服务缓存（跳过后台线程，同步可控）
    fn feed_native(
        svc: &mut ImageService,
        key: ImgKey,
        decoded: Decoded,
        native: Option<image::DynamicImage>,
        name: &str,
    ) {
        let hist = match &decoded {
            Decoded::Static(img) => Some(Histogram::compute(img)),
            Decoded::Anim(a) => a.first().map(|(f, _)| Histogram::compute(f)),
        };
        // 测试直喂（代次匹配）：pending 无该 key 时补占位，有则沿用真实代次
        let gen = match svc.pending.get(&key) {
            Some((_, _, g)) => *g,
            None => {
                svc.base_gen += 1;
                let g = svc.base_gen;
                svc.pending
                    .insert(key, (std::time::Instant::now(), IoClass::Decode, g));
                g
            }
        };
        let _ = svc.tx.send(ImgMsg::Ready {
            key,
            decoded,
            hist,
            native,
            name: name.to_owned(),
            orient: 1,
            class: IoClass::Decode,
            gen,
        });
        svc.poll();
    }

    #[test]
    fn natives_预算准入与逐出() {
        // 预算 8MB（单张准入 4MB）：850×850 RGBA ≈ 2.89MB 准入；shown 保护
        // 使张数 cap = 1+2 = 3，3 张 ≈ 8.67MB 超预算 → 预算护栏逐出最旧
        let mut svc = ImageService::new(
            egui::Context::default(),
            2,
            Default::default(),
            8 * 1024 * 1024,
            test_sched(),
        );
        svc.shown = Some((1, 5));
        let nat = |v: u8| {
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                850,
                850,
                image::Rgba([v, 0, 0, 255]),
            ))
        };
        for i in 0..6u8 {
            feed_native(
                &mut svc,
                (1, i as usize),
                Decoded::Static(egui::ColorImage::new([4, 4], vec![egui::Color32::WHITE; 16])),
                Some(nat(10 + i)),
                "t.png",
            );
        }
        assert!(
            svc.natives_bytes <= 8 * 1024 * 1024,
            "超预算必须逐出，实际 {} 字节",
            svc.natives_bytes
        );
        assert!(svc.natives.len() <= 3, "张数 cap = 栏数+2");
        assert!(
            svc.natives.iter().any(|(k, _)| *k == (1, 5)),
            "最新一张（且为当前显示）应保留"
        );
        assert!(
            svc.natives.iter().all(|(k, _)| k.1 >= 4),
            "最旧的应被逐出，剩余下标: {:?}",
            svc.natives.iter().map(|(k, _)| k.1).collect::<Vec<_>>()
        );
    }

    #[test]
    fn refresh_contrast_natives重建与直方图重算() {
        let map: std::rc::Rc<
            std::cell::RefCell<std::collections::HashMap<ImgKey, i32>>,
        > = Default::default();
        let mut svc = ImageService::new(egui::Context::default(), 2, map.clone(), 4 * 1024 * 1024, test_sched());
        let key = (7, 0);
        // 灰 100 平坦图：native 带回（预算内）→ refresh 毫秒级重建
        feed_native(
            &mut svc,
            key,
            Decoded::Static(egui::ColorImage::new(
                [8, 8],
                vec![egui::Color32::from_gray(100); 64],
            )),
            Some(image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                8,
                8,
                image::Rgba([100, 100, 100, 255]),
            ))),
            "t.png",
        );
        assert!(svc.frame(key).is_some(), "首帧应就绪");
        // 原值直方图全部落 bin 100（小图会被 finish_static ×2 上采样，
        // 像素总数取首帧实际值，不写死 64）
        let px_count = svc.histogram(key).expect("hist 应随上传就绪").ch[3][100];
        assert!(px_count > 0);
        // 调对比度 +2 步（×1.56，斜率 1.25^2）→ refresh_contrast 原位重建：
        // 灰 100 → (100−127.5)×1.5625+127.5 ≈ 85（低于锚点的值更暗）
        map.borrow_mut().insert(key, 2);
        assert!(svc.refresh_contrast(key, 4096), "natives 命中应返 true");
        let h1 = svc.histogram(key).expect("重建后 hist 应重算");
        assert_eq!(h1.ch[3][85], px_count, "全部像素应移到 LUT 后的 bin");
        assert_eq!(h1.ch[3][100], 0, "原 bin 应清空（直方图与显示帧一致）");
        assert!(svc.frame(key).is_some(), "重建后纹理应在位");
        // 复位 0 → 再次重建回原值
        map.borrow_mut().insert(key, 0);
        assert!(svc.refresh_contrast(key, 4096));
        let h2 = svc.histogram(key).expect("复位后 hist 应回原值");
        assert_eq!(h2.ch[3][100], px_count);
    }

    #[test]
    fn refresh_contrast_无native清缓存返false() {
        let mut svc = ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        let key = (7, 1);
        // 无 native（预算 0 → 准入关闭模拟巨图）：refresh 无原图可重建
        feed_native(
            &mut svc,
            key,
            Decoded::Static(egui::ColorImage::new([4, 4], vec![egui::Color32::WHITE; 16])),
            None,
            "t.png",
        );
        assert!(svc.frame(key).is_some());
        assert!(
            !svc.refresh_contrast(key, 4096),
            "natives 未命中应返 false（调用方重解码）"
        );
        assert!(svc.frame(key).is_none(), "旧纹理条目应被清掉防陈旧显示");
    }

    /// deferred 两遍放行：队首陈旧预取不 HOL 卡住后面的用户解码请求
    /// （第一遍只放 Decode/Full，第二遍才从队首放 Prefetch 垫底）
    #[test]
    fn deferred_两遍放行_预取让位用户请求() {
        let mut svc =
            ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        // 解码占槽（默认 max=1）：后续请求全部挂 deferred
        svc.sched.issue(IoClass::Decode);
        let src = ImageSource::File(std::path::PathBuf::from("nowhere.bin"));
        // 先排预取（队首），再排用户解码请求（队尾）——陈旧预取在前
        svc.request((9, 0), src.clone(), 0, "p.png", IoClass::Prefetch);
        svc.request((9, 1), src, 0, "u.png", IoClass::Decode);
        assert_eq!(svc.deferred.len(), 2, "槽位占满应挂 deferred");
        // 槽位释放 → poll 放行：第一遍越过队首 Prefetch 放行 Decode，
        // 第二遍 Prefetch admission 失败（解码在途）留在队首
        svc.sched.complete(IoClass::Decode);
        svc.poll();
        assert!(
            svc.pending.contains_key(&(9, 1)),
            "用户解码请求应先放行开工"
        );
        assert_eq!(svc.sched.decode_out(), 1);
        assert_eq!(svc.deferred.len(), 1, "陈旧预取不应先于用户请求放行");
        // 解码回账 → 再 poll：队首 Prefetch 放行
        svc.sched.complete(IoClass::Decode);
        svc.poll();
        assert!(svc.deferred.is_empty());
        assert!(svc.pending.contains_key(&(9, 0)), "预取放行开工");
        assert_eq!(svc.sched.prefetch_out(), 1);
    }

    /// 迟到 Ready 代次丢弃：invalidate 剔除旧代次占位后，旧 worker 迟到
    /// 产物按代次丢弃（不落缓存不吃掉新请求）；新代次产物正常落缓存
    #[test]
    fn 迟到ready_代次不符丢弃_新代次落缓存() {
        let mut svc =
            ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        let key = (9, 8);
        let img = || {
            Decoded::Static(egui::ColorImage::new(
                [4, 4],
                vec![egui::Color32::WHITE; 16],
            ))
        };
        // 旧代次 gen=1 占位 → invalidate 剔除（真实场景：热重载重放/旋转写回）
        svc.base_gen = 1;
        svc.pending
            .insert(key, (std::time::Instant::now(), IoClass::Decode, 1));
        svc.invalidate(key);
        assert!(!svc.pending.contains_key(&key));
        // 旧 worker 迟到 Ready（gen=1）：complete 照常回账（类随消息走），
        // 但产物按代次丢弃
        let _ = svc.tx.send(ImgMsg::Ready {
            key,
            decoded: img(),
            hist: None,
            native: None,
            name: "t.png".into(),
            orient: 1,
            class: IoClass::Decode,
            gen: 1,
        });
        svc.poll();
        assert!(svc.pos(key).is_none(), "代次不符的迟到产物必须丢弃");
        // 新代次请求占位 + 对应 Ready 正常落缓存
        svc.base_gen = 2;
        svc.pending
            .insert(key, (std::time::Instant::now(), IoClass::Decode, 2));
        let _ = svc.tx.send(ImgMsg::Ready {
            key,
            decoded: img(),
            hist: None,
            native: None,
            name: "t.png".into(),
            orient: 1,
            class: IoClass::Decode,
            gen: 2,
        });
        svc.poll();
        assert!(svc.pos(key).is_some(), "新代次产物应落缓存");
    }

    /// dispatch_full 单槽互斥：其它条目的 full 在途时不得抢槽/覆盖
    /// full_pending 记账（返回 true = 受理，槽腾出后触发方重触发自然开工）
    #[test]
    fn dispatch_full_单槽互斥_不覆盖在途() {
        let mut svc =
            ImageService::new(egui::Context::default(), 2, Default::default(), 0, test_sched());
        let busy_key = (9, 0);
        svc.full_pending = Some((busy_key, std::time::Instant::now(), true));
        let src = ImageSource::File(std::path::PathBuf::from("nowhere.bin"));
        let r = svc.dispatch_full((9, 1), src, "u.png", SharpenCfg::OFF);
        assert!(r, "槽位忙应返回 true（调用方认为已受理）");
        assert!(
            svc.full_pending
                .as_ref()
                .is_some_and(|(k, _, _)| *k == busy_key),
            "在途槽不得被覆盖"
        );
        assert_eq!(svc.sched.decode_out(), 0, "不得为第二个 full 发牌");
    }
}
