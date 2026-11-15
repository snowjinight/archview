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

use crate::vfs::{ArchiveVFS, VirtualEntry};
use egui::TextureOptions;
use image::AnimationDecoder;
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
const PENDING_TIMEOUT: Duration = Duration::from_secs(10);

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
    cfg.ui.image_texture && cfg.is_image_ext(name)
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
        let mut h = Self {
            ch: [[0; 256]; 4],
        };
        for px in &img.pixels {
            let (r, g, b, a) = (
                px.r() as u32,
                px.g() as u32,
                px.b() as u32,
                px.a() as u32,
            );
            let (r, g, b) = if a < 255 && a > 0 {
                (
                    r * 255 / a,
                    g * 255 / a,
                    b * 255 / a,
                )
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
    },
    Failed {
        key: ImgKey,
        error: String,
    },
    /// full 槽纹理就绪（原图 ×ppp×3，见 request_full）
    FullReady {
        key: ImgKey,
        img: egui::ColorImage,
        name: String,
    },
    /// 原图解码失败/放弃：UI 保持降采样纹理
    FullFailed {
        key: ImgKey,
    },
}

/// LRU 缓存条目：逐帧纹理 + 播放状态（静态图 = 单帧无时钟）
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

pub struct ImageService {
    tx: Sender<ImgMsg>,
    rx: Receiver<ImgMsg>,
    ctx: egui::Context,
    /// 解码中集合：发起时刻 → 看门狗超时判定。防重复 spawn 同一条目
    /// （请求 → 线程 → Ready/Failed 才移除）
    pending: HashMap<ImgKey, Instant>,
    /// LRU 纹理缓存（front = 最近使用；back = 最久未用，逐出候选）
    cache: VecDeque<(ImgKey, Cached)>,
    cap: usize,
    /// 最近一次成功显示的条目：逐出保护。滚轮连翻时每跳 ±1 预取的推入
    /// 洪水会把正在显示的条目从队尾挤出（cap 小时必现），导致该图
    /// frame()=None 而 pending 已空 → 永久停在"正在解码图片…"占位符
    shown: Option<ImgKey>,
    /// 对比模式显示集合：多栏同时显示时全部受逐出保护（预取洪水不挤掉
    /// 任何一栏）；单图模式恒空，与 shown 互不干扰
    shown_multi: Vec<ImgKey>,
    /// 原图纹理单槽：大图放大到 1:1 附近时按需解码不降采样版本无缝换画。
    /// 单槽不进 LRU——原图显存大（12288 最长边 + 100MP 预算，峰值
    /// ~400MB），只随当前显示条目存续
    /// （frame() 切条目即弃）
    full: Option<(ImgKey, egui::TextureHandle)>,
    /// 原图解码中（key + 发起时刻，后者供看门狗超时判定）
    full_pending: Option<(ImgKey, Instant)>,
    /// 原图解码放弃标记（失败/超时/动图）：防止每帧重触发无限重解
    full_failed: Option<ImgKey>,
    /// 原图单槽缓存（base worker 顺手留存，仅静态且具备 full 资格的图）：
    /// full 升级复用免二次解码（2K 图省 ~300ms PNG 解码）。上限 ~30MB
    /// （native ≤ image_max_dim/ppp，正方形极端 30MB），随下一张图入槽/
    /// full 就绪/失败自然更替
    native: Option<(ImgKey, image::DynamicImage)>,
}

impl ImageService {
    pub fn new(ctx: egui::Context, cap: usize) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            ctx,
            pending: HashMap::new(),
            cache: VecDeque::new(),
            cap: cap.max(1),
            shown: None,
            shown_multi: Vec::new(),
            full: None,
            full_pending: None,
            full_failed: None,
            native: None,
        }
    }

    /// 更新 LRU 容量（config 热重载同步 ui.image_cache_count）。
    /// 收缩在下次推入时生效（逐出按本字段比较，已缓存内容不主动丢弃）
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
    }

    /// 对比模式显示集合（逐出保护；退出对比时清空）
    pub fn set_shown_multi(&mut self, keys: Vec<ImgKey>) {
        self.shown_multi = keys;
    }

    pub fn clear_shown_multi(&mut self) {
        self.shown_multi.clear();
    }

    /// 请求解码（幂等：已缓存/解码中直接返回）。
    /// 调用方负责构造 source；max_dim 来自 cfg.ui.image_max_dim；
    /// name 仅用于日志（定位"发起了没/何时发起"）
    pub fn request(&mut self, key: ImgKey, source: ImageSource, max_dim: u32, name: &str) {
        if self.pending.contains_key(&key) || self.pos(key).is_some() {
            return;
        }
        tracing::debug!("发起解码 {name} (在途 {})", self.pending.len());
        // 基础档预放大倍率 = 屏幕 DPI（ppp）：150% 缩放屏 ×1.5。低密屏
        // （ppp<1）钳 1.0 免无谓缩小重采样。取发起时刻值；跨屏迁移后
        // 已缓存纹理不回炉（LRU 深度浅，翻几页即换血）
        let factor = (self.ctx.pixels_per_point() as f64).max(1.0);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let name = name.to_owned();
        self.pending.insert(key, Instant::now());
        let r = std::thread::Builder::new()
            .name("imgdec".into())
            .spawn(move || {
                // 解码对畸形文件可能 panic（坏图触发 codec 内部越界等）：
                // 捕获转 Failed。否则线程静默死亡（无 send、无 repaint），
                // pending 永久泄漏 → 该图被幂等检查拦截 → 永久占位死局
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_image(&source, max_dim, false, factor, &name, &SharpenCfg::OFF)
                }));
                match r {
                    Ok(Ok((decoded, native))) => {
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
                        });
                    }
                    Ok(Err(e)) => {
                        let _ = tx.send(ImgMsg::Failed { key, error: e });
                    }
                    Err(_) => {
                        let _ = tx.send(ImgMsg::Failed {
                            key,
                            error: "解码线程 panic".into(),
                        });
                    }
                }
                // 唤醒 UI 立即 poll：否则要等下一次重绘（空闲时可迟 100ms+，
                // 实测 LOG 解码完成→上帧间隔 50~130ms 全是这个）
                ctx.request_repaint();
            });
        if r.is_err() {
            // 线程起不来（极端）：移除 pending 让后续请求可重试
            self.pending.remove(&key);
        }
    }

    /// 请求解码高清纹理（full 单槽，clamp FULL_MAX_DIM + 像素预算
    /// FULL_MAX_PIXELS）：深放大按需升级，原图 Lanczos ×(ppp×3)（150%
    /// 缩放屏 ×4.5，badge 300% 处 1:1 采样）——绝不在基础纹理（已 ×ppp）
    /// 上级联再放大（两级重采样抹锐边）。倍率内部按屏幕 DPI 取值，调用方
    /// 无需传参。
    /// 原图源：base worker 留存的单槽缓存命中即复用（免二次解码，2K 图省
    /// ~300ms）；未命中（切图回跳/缓存被换）从源重新解码。
    /// 升级资格由触发方保证：仅 native ≤ image_max_dim 的图（即基础
    /// 纹理走了预放大）值得升级；超大图原尺寸直进 LRU、深放大交给 GPU
    /// 双线性（二次解码秒级 + 300MB 级显存，增益 <×1.4 不值当）。
    /// 就绪后无缝换画
    /// （几何不变：屏幕矩形按基础纹理算，UV 全图映射对两种分辨率通用）。
    /// 幂等：已就绪/解码中/已放弃直接返回；动图不升级（全帧原图显存
    /// 不可控，触发方已用 is_static 前置过滤，单帧动图落到这里按
    /// FullFailed 放弃即可）
    /// sharpen 直传 config ui.image_full_sharpen*（默认开+0.6+σ1.0+降噪）：
    /// 放大前在原图分辨率执行（观感增强，成本 ~50ms，见 sharpen_full）
    pub fn request_full(
        &mut self,
        key: ImgKey,
        source: ImageSource,
        name: &str,
        sharpen: SharpenCfg,
    ) {
        if self.has_full(key) || self.is_full_pending(key) || self.is_full_failed(key) {
            return;
        }
        tracing::debug!("发起高清解码 {name}");
        // 高清档倍率 = ppp×3：badge（zoom×ppp）300% 处 full 纹素与物理像素
        // 1:1，300%~400% 主用区间锐度完整；200%~300% 为 0.67~1× 轻微缩采样
        // （静止图无感）。默认阈值 2.0（badge 200% 滚轮档）起接管
        let factor = (self.ctx.pixels_per_point() as f64).max(1.0) * 3.0;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let name = name.to_owned();
        self.full_pending = Some((key, Instant::now()));
        // 原图缓存命中即取走（槽随 full 就绪/失败释放）；未命中走二次解码
        let cached = if self.native.as_ref().is_some_and(|(k, _)| *k == key) {
            self.native.take().map(|(_, n)| n)
        } else {
            None
        };
        let r = std::thread::Builder::new()
            .name("imgfull".into())
            .spawn(move || {
                // 同 request：畸形文件可能 panic，捕获转 FullFailed 防死局
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match cached {
                    Some(img) => {
                        tracing::debug!("高清解码复用原图缓存{} {name}", sharpen.tag());
                        Ok(build_full(img, factor, &sharpen))
                    }
                    None => match decode_image(&source, FULL_MAX_DIM, true, factor, &name, &sharpen)
                    {
                        Ok((Decoded::Static(img), _)) => Ok(img),
                        // 单帧动图取唯一帧当静态原图
                        Ok((Decoded::Anim(mut a), _)) if a.len() == 1 => {
                            Ok(a.pop().expect("len==1").0)
                        }
                        Ok(_) | Err(_) => Err(()),
                    },
                }));
                let msg = match r {
                    Ok(Ok(img)) => ImgMsg::FullReady { key, img, name },
                    Ok(Err(())) | Err(_) => ImgMsg::FullFailed { key },
                };
                let _ = tx.send(msg);
                ctx.request_repaint();
            });
        if r.is_err() {
            self.full_pending = None;
        }
    }

    /// 每帧排空回传通道：上传纹理进 LRU，收集事件
    pub fn poll(&mut self) -> Vec<ImgEvent> {
        // 看门狗：pending 超时（线程卡死/消息丢失的兜底）——剔除允许
        // 下次 play/预取重新发起，否则该图永久占位
        let now = Instant::now();
        let expired: Vec<ImgKey> = self
            .pending
            .iter()
            .filter(|(_, t)| now.duration_since(**t) > PENDING_TIMEOUT)
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            self.pending.remove(&k);
            tracing::warn!("解码超时（>{PENDING_TIMEOUT:?}）剔除，可重试: {k:?}");
        }
        // 原图看门狗：超时判死并标记放弃（区别于基础解码的可重试——重触发
        // 条件 zoom≥0.95 在显示期间恒成立，可重试会无限重解卡死图）
        if let Some((k, t0)) = self.full_pending {
            if now.duration_since(t0) > PENDING_TIMEOUT {
                tracing::warn!("原图解码超时（>{PENDING_TIMEOUT:?}）放弃: {k:?}");
                self.full_pending = None;
                self.full_failed = Some(k);
            }
        }
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                ImgMsg::Ready { key, decoded, hist, name, native } => {
                    self.pending.remove(&key);
                    // 原图缓存：随 Ready 入槽替换旧图（None = 动图/超大图，
                    // 清旧槽防陈图驻留）
                    self.native = native.map(|n| (key, n));
                    // 计时：与解码日志对照，量化"解码 vs 纹理上传"占比
                    let t = std::time::Instant::now();
                    let (frames, delays) = match decoded {
                        Decoded::Static(image) => {
                            let tex = self.tex(&key, 0, image);
                            (vec![tex], vec![Duration::ZERO])
                        }
                        Decoded::Anim(anim) => {
                            let delays = anim.iter().map(|(_, d)| *d).collect::<Vec<_>>();
                            let frames = anim
                                .into_iter()
                                .enumerate()
                                .map(|(i, (img, _))| self.tex(&key, i, img))
                                .collect::<Vec<_>>();
                            (frames, delays)
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
                ImgMsg::Failed { key, error } => {
                    self.pending.remove(&key);
                    out.push(ImgEvent::Failed(key, error));
                }
                ImgMsg::FullReady { key, img, name } => {
                    // 只认当前 pending 的 key：槽已换条目/超时判死的迟到消息丢弃
                    if self.full_pending.take().is_some_and(|(k, _)| k == key) {
                        let t = std::time::Instant::now();
                        let tex = self.ctx.load_texture(
                            format!("img-full-{}-{}", key.0, key.1),
                            img,
                            TEX_OPTS_FULL,
                        );
                        tracing::debug!("纹理上传[{name}] full: {}ms", t.elapsed().as_millis());
                        self.full = Some((key, tex));
                        // 原图缓存使命完成（full 已建成），释放 ~30MB
                        self.native = None;
                    }
                }
                ImgMsg::FullFailed { key } => {
                    if self.full_pending.take().is_some_and(|(k, _)| k == key) {
                        tracing::warn!("原图解码失败，保持降采样纹理: {key:?}");
                        self.full_failed = Some(key);
                        self.native = None;
                    }
                }
            }
        }
        out
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
        self.full_pending.is_some_and(|(k, _)| k == key)
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

/// 后台线程主体：读源 → 解码（gif/webp 逐帧）→ 尺寸适配 → RGBA。
/// full=false 基础档（factor=ppp）/ true 高清档（factor=ppp×3），
/// 两档策略见 resample；factor 由 ImageService 按屏幕 DPI 内部取值；
/// sharpen = 高清档放大前观感增强配置（config ui.image_full_sharpen*，
/// 基础档传 [`SharpenCfg::OFF`]）
/// 返回 (结果, 原图保留)：原图仅基础档静态且具备 full 资格（native×factor
/// ≤ max_dim，同预放大判据）时带回，供 full 升级复用免二次解码
fn decode_image(
    source: &ImageSource,
    max_dim: u32,
    full: bool,
    factor: f64,
    name: &str,
    sharpen: &SharpenCfg,
) -> Result<(Decoded, Option<image::DynamicImage>), String> {
    match source {
        ImageSource::File(path) => {
            // 相机 RAW 拦截：with_guessed_format 对 ARW/NEF 探不出格式
            //（返回 None），须按扩展名先行拦截（方案 §4.3）。失败 fall
            // through 原路径——改名伪 raw 由 ImageReader 报错 → mpv 回退
            if is_raw_photo_ext(name) {
                if let Ok(img) = decode_raw_preview_path(path) {
                    tracing::debug!("RAW 预览 [{name}] {}x{}", img.width(), img.height());
                    return finish_static(img, max_dim, full, factor, sharpen);
                }
            }
            let reader = image::ImageReader::open(path)
                .map_err(|e| format!("打开图片失败: {e}"))?
                .with_guessed_format()
                .map_err(|e| format!("识别图片格式失败: {e}"))?;
            decode_any(reader, max_dim, full, factor, name, sharpen)
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
                if let Ok(img) = decode_raw_preview(&buf) {
                    tracing::debug!("RAW 预览 [{name}] {}x{}", img.width(), img.height());
                    return finish_static(img, max_dim, full, factor, sharpen);
                }
            }
            let reader = image::ImageReader::new(std::io::Cursor::new(buf))
                .with_guessed_format()
                .map_err(|e| format!("识别图片格式失败: {e}"))?;
            decode_any(reader, max_dim, full, factor, name, sharpen)
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
) -> Result<(Decoded, Option<image::DynamicImage>), String> {
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
        // 动图（含单帧）不保留原图：不做 full 升级，缓存白驻
        return Ok(if frames.len() == 1 {
            let (img, _) = frames.into_iter().next().expect("len==1");
            (Decoded::Static(img), None)
        } else {
            (Decoded::Anim(frames), None)
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
            // 同 gif：动图（含单帧）不保留原图
            return Ok(if frames.len() == 1 {
                let (img, _) = frames.into_iter().next().expect("len==1");
                (Decoded::Static(img), None)
            } else {
                (Decoded::Anim(frames), None)
            });
        }
        image::DynamicImage::from_decoder(dec).map_err(|e| format!("解码图片失败: {e}"))?
    } else {
        reader.decode().map_err(|e| format!("解码图片失败: {e}"))?
    };
    // 两档尺寸适配（策略见 resample）：基础档 LRU 存 native×ppp（翻页
    // 即 100% 密度直出）；高清档 full 槽存 native×ppp×3（深放大用，
    // clamp 12288 + 像素预算）。factor 由 ImageService 按屏幕 DPI 定。
    // 共享尾部抽为 finish_static，与 RAW 预览拦截分支共用（方案 §4.3）
    let (w, h) = (img.width(), img.height());
    let (decoded, native) = finish_static(img, max_dim, full, factor, sharpen)?;
    // 尺寸命名日志：native_size 原图 / lru_size 基础纹理 / full_size
    // full 槽纹理（箭头一致 = 未触发重采样，尺寸原样直进）
    let Decoded::Static(out) = &decoded else {
        unreachable!("finish_static 恒返回 Static")
    };
    tracing::debug!(
        "解码 {fmt:?} [{base}] native_size={w}x{h} {}_size={}x{}{}: {}ms",
        if full { "full" } else { "lru" },
        out.size[0],
        out.size[1],
        if full { sharpen.tag() } else { "" },
        t.elapsed().as_millis()
    );
    Ok((decoded, native))
}

/// 静态图共享尾部：可选降噪+锐化（config ui.image_full_sharpen*，仅高清
/// 档，**放大前**在原图分辨率执行——把边缘坡度先变陡，Lanczos 插值保持
/// 陡峭过渡，成本从放大后千万像素级降到原图级，见 [`sharpen_full`]）→
/// 两档尺寸适配（见 [`resample`]）→ RGBA，并按 full 资格判据
///（native×factor ≤ max_dim，同基础档预放大阈值）决定是否保留原图供 full
/// 升级复用。[`decode_any`] 与 RAW 预览拦截分支共用
fn finish_static(
    img: image::DynamicImage,
    max_dim: u32,
    full: bool,
    factor: f64,
    sharpen: &SharpenCfg,
) -> Result<(Decoded, Option<image::DynamicImage>), String> {
    let img = if full && sharpen.on { sharpen_full(img, sharpen) } else { img };
    // 原图保留（仅基础档）：full=true 时原图即输入，无需缓存
    let native = if full {
        None
    } else {
        let longest = img.width().max(img.height());
        ((longest as f64 * factor).round() <= max_dim as f64).then(|| img.clone())
    };
    let out = resample(img, max_dim, full, factor);
    Ok((Decoded::Static(color_image(out)), native))
}

/// full 槽构建（原图缓存命中路径，免读源）：可选降噪+锐化（原图分辨率）
/// → ×factor Lanczos 上采样（clamp + 像素预算）→ RGBA。与解码路径的
/// finish_static 同语义
fn build_full(img: image::DynamicImage, factor: f64, sharpen: &SharpenCfg) -> egui::ColorImage {
    let img = if sharpen.on { sharpen_full(img, sharpen) } else { img };
    color_image(resample(img, FULL_MAX_DIM, true, factor))
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
/// 看门狗机制照常生效。识别/解码失败返回 Err，调用方 fall through
fn decode_raw_preview(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    let mut f = raw_files::RawFile::from_bytes(bytes)
        .map_err(|e| format!("RAW 识别失败: {e}"))?;
    let pv = f.largest_preview().ok_or("RAW 无内嵌预览")?;
    let jpeg = f.read_preview(&pv).map_err(|e| format!("读预览失败: {e}"))?;
    let img = image::load_from_memory(&jpeg).map_err(|e| format!("预览解码失败: {e}"))?;
    Ok(apply_orientation(img, pv.orientation()))
}

/// 同 [`decode_raw_preview`]，文件源（7z/RAR 落盘缓存）：`RawFile::open`
/// 的块缓存只读文件结构与预览所在块，优于全量读盘
fn decode_raw_preview_path(path: &std::path::Path) -> Result<image::DynamicImage, String> {
    let mut f = raw_files::RawFile::open(path).map_err(|e| format!("RAW 识别失败: {e}"))?;
    let pv = f.largest_preview().ok_or("RAW 无内嵌预览")?;
    let jpeg = f.read_preview(&pv).map_err(|e| format!("读预览失败: {e}"))?;
    let img = image::load_from_memory(&jpeg).map_err(|e| format!("预览解码失败: {e}"))?;
    Ok(apply_orientation(img, pv.orientation()))
}

/// EXIF 方向矫正：raw-files `Orientation`（EXIF 1-8，变体名与 image
/// crate 一一对应）→ image crate 旋转/翻转。EXIF 恰定义 8 个值，
/// 穷尽匹配无需兜底。镜像/180° 用 in_place 零分配；90/270 旋转需
/// 重排行列返回新图；EXIF 5/7 = 旋转后左右镜像（主/反对角线转置）
fn apply_orientation(img: image::DynamicImage, o: raw_files::Orientation) -> image::DynamicImage {
    use image::imageops;
    use raw_files::Orientation as O;
    match o {
        O::NoTransforms => img,
        O::FlipHorizontal => {
            let mut i = img;
            imageops::flip_horizontal_in_place(&mut i);
            i
        }
        O::Rotate180 => {
            let mut i = img;
            imageops::rotate180_in_place(&mut i);
            i
        }
        O::FlipVertical => {
            let mut i = img;
            imageops::flip_vertical_in_place(&mut i);
            i
        }
        // EXIF 5：顺时针 90° 后左右镜像（主对角线转置）。
        // image 0.25 的 imageops 旋转/镜像自由函数返回 ImageBuffer
        //（非 I），统一 to_rgba8 再包回 DynamicImage
        O::Rotate90FlipH => image::DynamicImage::ImageRgba8(imageops::flip_horizontal(
            &imageops::rotate90(&img.to_rgba8()),
        )),
        // EXIF 6：顺时针 90°
        O::Rotate90 => image::DynamicImage::ImageRgba8(imageops::rotate90(&img.to_rgba8())),
        // EXIF 7：顺时针 270° 后左右镜像（反对角线转置）
        O::Rotate270FlipH => image::DynamicImage::ImageRgba8(imageops::flip_horizontal(
            &imageops::rotate270(&img.to_rgba8()),
        )),
        // EXIF 8：顺时针 270°
        O::Rotate270 => image::DynamicImage::ImageRgba8(imageops::rotate270(&img.to_rgba8())),
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
    /// unsharp 强度（config image_full_sharpen_amount，0~2，默认 0.45）：
    /// out = px + amount×(px − blur(px))，越大边缘越脆、光晕与噪点放大越重
    pub amount: f32,
    /// unsharp 半径 σ 像素（config image_full_sharpen_sigma，0.1~4，默认
    /// 1.5）：调大只增强中大尺度边缘（σ 级以下的像素噪点被模糊项抹掉、
    /// 差值趋零），天然抗噪点放大，代价是锐化"颗粒感"变"体积感"
    pub sigma: f32,
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
    /// 默认 10，仅 Guided 生效）：内部换算 ε = D²，方差 ≪ ε 被平均（压噪）、
    /// ≫ ε 原值保留（边缘/渐变）——与双边 σc 同量纲，同值可比
    pub guided_strength: f32,
    /// 导向滤波窗口半径（1~8，config image_full_guided_radius，默认 2 = 5×5，
    /// 仅 Guided 生效）：越大降噪越强、弱纹理抹除也越强
    pub guided_radius: u32,
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
    /// config ui 段 → 观感增强配置（panels 两处组装点收敛到此；algo 非法
    /// 值 WARN 回退导向，不静默）
    pub fn from_ui(ui: &crate::config::UiConfig) -> Self {
        let (algo, ok) = DenoiseAlgo::parse(&ui.image_full_denoise_algo);
        if !ok {
            tracing::warn!(
                "image_full_denoise_algo = {:?} 无效（仅 bilateral/guided），回退导向",
                ui.image_full_denoise_algo
            );
        }
        Self {
            on: ui.image_full_sharpen,
            amount: ui.image_full_sharpen_amount,
            sigma: ui.image_full_sharpen_sigma,
            denoise: ui.image_full_denoise,
            algo,
            bilateral_strength: ui.image_full_bilateral_strength,
            guided_strength: ui.image_full_guided_strength,
            guided_radius: ui.image_full_guided_radius,
        }
    }

    /// 关闭档（基础档解码/测试用）
    pub const OFF: Self = Self {
        on: false,
        amount: 0.0,
        sigma: 0.0,
        denoise: false,
        algo: DenoiseAlgo::Bilateral,
        bilateral_strength: 0.0,
        guided_strength: 0.0,
        guided_radius: 0,
    };

    /// 降噪+锐化的日志标记（空 = 关；降噪带算法名，A/B 对比时看日志即可
    /// 分辨本次解码走了哪条链）
    fn tag(&self) -> &'static str {
        if !self.on {
            return "";
        }
        match self.denoise {
            true => match self.algo {
                DenoiseAlgo::Bilateral => " 降噪[双边]+锐化",
                DenoiseAlgo::Guided => " 降噪[导向]+锐化",
            },
            false => " 锐化",
        }
    }
}

/// 高清档观感增强链（仅高清档，config ui.image_full_sharpen*，默认
/// 开+0.6+1.0）：可选降噪（算法 [`DenoiseAlgo`] 切换）→ unsharp mask——
/// **放大前在原图分辨率**
/// 执行：先把边缘坡度变陡，Lanczos 插值按比例保持陡峭过渡，观感近似
/// "放大后锐化"而成本从放大后千万像素级（~400ms 高斯模糊）降到原图级
/// （2K 图降噪+锐化 ~50ms，被 74MP Lanczos 主耗时掩盖）。实测锐化对深
/// 放大观感增益明显（用户确认默认开）；噪点同步放大由前置降噪对冲。
/// 仅 RGB，alpha 直通。O(n) 后台线程执行
fn sharpen_full(img: image::DynamicImage, cfg: &SharpenCfg) -> image::DynamicImage {
    let mut rgba = img.to_rgba8();
    if cfg.denoise {
        rgba = match cfg.algo {
            DenoiseAlgo::Bilateral => {
                bilateral_3x3(&rgba, cfg.bilateral_strength.clamp(0.1, 100.0))
            }
            DenoiseAlgo::Guided => guided_denoise(&rgba, cfg.guided_strength, cfg.guided_radius),
        };
    }
    let amount = cfg.amount.clamp(0.0, 2.0);
    let sigma = cfg.sigma.clamp(0.1, 4.0);
    let blurred = image::imageops::blur(&rgba, sigma);
    // 过冲钳制（A/B 实测驱动）：unsharp 的差值项在强边处很大（σ1 下阶跃边
    // 缘 diff ~±60），×amount 后光晕 20~40 级，×4.5 放大后在头发等密集细边
    // 区呈明显亮线/暗线（用户实测两算法均有光晕）。**钳差值不钳输出**——
    // 细纹理 diff（±5~15）不受影响锐度不变，只把强边过冲封顶在
    // ±HALO_CLAMP×amount（~±12 级，保留少量锐感）。经典 smart-sharpen 做法
    const HALO_CLAMP: f32 = 20.0;
    for (p, b) in rgba.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let d = (p[c] as f32 - b[c] as f32).clamp(-HALO_CLAMP, HALO_CLAMP);
            let v = p[c] as f32 + amount * d;
            p[c] = v.clamp(0.0, 255.0) as u8;
        }
    }
    image::DynamicImage::ImageRgba8(rgba)
}

/// 3×3 双边滤波（保边降噪，锐化前置）：空间权重固定 3×3 高斯 σs=1.0，
/// 颜色权重按中心与邻点 RGB 亮度差的指数衰减——`sigma_c`（亮度级 0~100）
/// 是颜色容忍度：亮度差小于它的邻点参与平均。sigma_c 越大平均越猛，皮肤
/// 渐变/毛孔等弱纹理也被抹掉（涂抹感）；越小只压最细噪点、弱纹理全保。
/// 亮度差用 RGB 感知加权（Rec.601 系数），三通道共用同一权重保持色彩不
/// 偏。中心权重恒 1.0 无除零；O(n·9) 纯 Rust，2K 图 ~30ms 后台执行
fn bilateral_3x3(img: &image::RgbaImage, sigma_c: f32) -> image::RgbaImage {
    // 3×3 高斯 σs=1.0：exp(-0.5/2σs²)=0.6065（正交）、exp(-1/2σs²)=0.3679（对角）
    const SW: [f32; 9] = [0.3679, 0.6065, 0.3679, 0.6065, 1.0, 0.6065, 0.3679, 0.6065, 0.3679];
    let (w, h) = (img.width() as i64, img.height() as i64);
    let src = img.as_raw();
    let lum = |px: &[u8]| f32::from(px[0]) * 0.299 + f32::from(px[1]) * 0.587 + f32::from(px[2]) * 0.114;
    let mut out = Vec::with_capacity(src.len());
    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;
            let c = &src[idx..idx + 4];
            let lc = lum(c);
            let (mut r, mut g, mut b, mut wsum) = (0f32, 0f32, 0f32, 0f32);
            let mut k = 0;
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    // 边界钳位到图内（边缘邻点重复采样，简单且够用）
                    let (ny, nx) = ((y + dy).clamp(0, h - 1), (x + dx).clamp(0, w - 1));
                    let j = ((ny * w + nx) * 4) as usize;
                    let p = &src[j..j + 4];
                    let d = lum(p) - lc;
                    let wgt = SW[k]
                        * (-(d * d) / (2.0 * sigma_c * sigma_c)).exp();
                    r += wgt * f32::from(p[0]);
                    g += wgt * f32::from(p[1]);
                    b += wgt * f32::from(p[2]);
                    wsum += wgt;
                    k += 1;
                }
            }
            out.push((r / wsum).clamp(0.0, 255.0) as u8);
            out.push((g / wsum).clamp(0.0, 255.0) as u8);
            out.push((b / wsum).clamp(0.0, 255.0) as u8);
            out.push(c[3]);
        }
    }
    image::RgbaImage::from_raw(img.width(), img.height(), out).expect("缓冲与尺寸自洽")
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
/// 输出只取平滑后的亮度校正量 Δ = q − I 加回 RGB 三通道（色度比例原样
/// 保留，与双边"RGB 共用亮度权重"同效）。alpha 直通。边缘 clamp 重复采样
fn guided_denoise(img: &image::RgbaImage, strength: f32, radius: u32) -> image::RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let r = radius.clamp(1, 8) as usize;
    let eps = strength.clamp(0.1, 100.0) * strength.clamp(0.1, 100.0);
    let src = img.as_raw();
    // Rec.601 亮度（与 bilateral_3x3 的 lum 同系数，两链路语义对齐）
    let lum: Vec<f32> = src
        .chunks_exact(4)
        .map(|p| f32::from(p[0]) * 0.299 + f32::from(p[1]) * 0.587 + f32::from(p[2]) * 0.114)
        .collect();
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
    // q = mean_a·I + mean_b → Δ = q − I 加回 RGB（逐像素，无需 q 缓冲）
    let mut out = Vec::with_capacity(src.len());
    for (p, i) in src.chunks_exact(4).zip(lum.iter()) {
        let delta = mean_a[out.len() / 4] * i + mean_b[out.len() / 4] - i;
        out.push((f32::from(p[0]) + delta).clamp(0.0, 255.0) as u8);
        out.push((f32::from(p[1]) + delta).clamp(0.0, 255.0) as u8);
        out.push((f32::from(p[2]) + delta).clamp(0.0, 255.0) as u8);
        out.push(p[3]);
    }
    image::RgbaImage::from_raw(img.width(), img.height(), out).expect("缓冲与尺寸自洽")
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

    fn test_cfg(image_texture: bool) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.ui.image_texture = image_texture;
        c
    }

    /// 开启锐化的测试配置（denoise 关：降噪滤波对合成阶跃图基本不动，
    /// 测试只验 unsharp 行为）
    fn sharpen_on(amount: f32) -> SharpenCfg {
        SharpenCfg {
            on: true,
            amount,
            sigma: 1.0,
            denoise: false,
            algo: DenoiseAlgo::Bilateral,
            bilateral_strength: 15.0,
            guided_strength: 15.0,
            guided_radius: 2,
        }
    }

    #[test]
    fn box_filter_滑动均值与朴素参考一致() {
        // 小随机图全量比对朴素 O(n·win²) 双重 clamp 窗口——钉死滑窗增量
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
        // 方差 ~3600 ≫ ε → a→1、q→I 原值保留
        let (w, h) = (48, 16);
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let base: i16 = if (20..28).contains(&x) { 200 } else { 80 };
                let noise: i16 = if (20..28).contains(&x) {
                    0
                } else {
                    ((x * 7 + y * 13) % 23) as i16 - 11
                };
                let v = (base + noise).clamp(0, 255) as u8;
                img.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        let lum = |im: &image::RgbaImage, x: u32, y: u32| {
            let p = im.get_pixel(x, y);
            f32::from(p[0]) * 0.299 + f32::from(p[1]) * 0.587 + f32::from(p[2]) * 0.114
        };
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
        let noise_energy = |im: &image::RgbaImage| {
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
        assert_eq!(DenoiseAlgo::parse("bilateral"), (DenoiseAlgo::Bilateral, true));
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
        let guided = sharpen_full(src.clone(), &cfg).to_rgba8();
        cfg.algo = DenoiseAlgo::Bilateral;
        let bilateral = sharpen_full(src, &cfg).to_rgba8();
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
        let out = sharpen_full(flat, &sharpen_on(0.6)).to_rgba8();
        assert!(out.pixels().all(|p| p[0] == 128));
        // 垂直阶跃：左 64 右 192（取中灰区间防 0/255 饱和掩盖效果），
        // 边缘内侧低侧压暗、高侧提亮 = 坡度变陡
        let mut img = image::GrayImage::new(16, 8);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 8 { 64 } else { 192 }]);
            }
        }
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img), &sharpen_on(0.6)).to_rgba8();
        assert!(out.get_pixel(7, 4)[0] < 64, "边缘低侧应压暗");
        assert!(out.get_pixel(8, 4)[0] > 192, "边缘高侧应提亮");
    }

    #[test]
    fn sharpen_full_过冲钳制() {
        // 垂直阶跃（128 级差）：无钳制时强边增量 ~±40 级（0.6×diff60+），
        // 钳制后增量封顶 amount×20 = 12 级——强边光晕可见地压制
        let mut img = image::GrayImage::new(16, 8);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 8 { 64 } else { 192 }]);
            }
        }
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img), &sharpen_on(0.6)).to_rgba8();
        let hi = i32::from(out.get_pixel(8, 4)[0]) - 192;
        let lo = 64 - i32::from(out.get_pixel(7, 4)[0]);
        assert!(hi <= 13, "强边过冲未钳住: 高侧 +{hi}");
        assert!(lo <= 13, "强边过冲未钳住: 低侧 −{lo}");
        assert!(hi >= 5 && lo >= 5, "过冲全灭=强边锐感丢失: +{hi}/−{lo}");
        // 细纹理不受钳制影响：幅度 8 的单像素凸起，diff ≤ 8 < 20 →
        // 增量 = amount×diff 全额保留
        let mut img = image::GrayImage::from_pixel(16, 16, image::Luma([128]));
        *img.get_pixel_mut(8, 8) = image::Luma([136]);
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img), &sharpen_on(0.6)).to_rgba8();
        let up = i32::from(out.get_pixel(8, 8)[0]) - 128;
        assert!(up >= 3, "细纹理锐化被钳伤: +{up}");
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

    /// 读取仓库根 testdata/ 下的测试夹具（二进制夹具不入 src/）。
    fn read_testdata(name: &str) -> Option<Vec<u8>> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        std::fs::read(p).ok()
    }

    #[test]
    fn decode_动画webp逐帧() {
        // 夹具：ffmpeg libwebp 生成的 2 帧动画（红/蓝 20×14，100ms/帧，
        // VP8X ANIM 位置位 + ANIM/ANMF 块）——has_animation 路由进 gif
        // 同款逐帧机制；动图不预放大，帧为画布原尺寸。
        // 夹具在仓库根 testdata/anim2.webp，缺失时跳过；生成命令：
        //   ffmpeg -f lavfi -i color=c=red:s=20x14  -frames:v 1 f0.png
        //   ffmpeg -f lavfi -i color=c=blue:s=20x14 -frames:v 1 f1.png
        //   ffmpeg -framerate 10 -i f%d.png -loop 0 testdata/anim2.webp
        let Some(bytes) = read_testdata("anim2.webp") else {
            eprintln!("跳过：testdata/anim2.webp 不存在（生成命令见本测试注释）");
            return;
        };
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
        let _ = svc.tx.send(ImgMsg::Ready {
            key,
            decoded,
            hist,
            native: None,
            name: name.to_owned(),
        });
        svc.poll();
    }

    #[test]
    fn lru_容量逐出_跳过当前() {
        let mut svc = ImageService::new(egui::Context::default(), 2);
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
        let mut svc = ImageService::new(egui::Context::default(), 2);
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
        let mut svc = ImageService::new(egui::Context::default(), 2);
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
        let mut svc = ImageService::new(egui::Context::default(), 2);
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
        let mut svc = ImageService::new(egui::Context::default(), 3);
        let missing = || ImageSource::File(std::path::PathBuf::from("definitely_missing.png"));
        // 同 key 重复请求只解码一次
        svc.request((9, 9), missing(), 4096, "t.png");
        svc.request((9, 9), missing(), 4096, "t.png");
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
        // pending 清除后可重新请求（重试语义）
        svc.request(
            (9, 9),
            ImageSource::File(std::path::PathBuf::from("x.png")),
            4096,
            "t.png",
        );
        assert_eq!(svc.pending.len(), 1);
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
        assert!(decode_raw_preview(b"definitely not a camera raw file").is_err());
        assert!(decode_raw_preview(&[]).is_err());
    }

    #[test]
    fn apply_orientation_八个exif方向映射() {
        use image::GenericImageView;
        use raw_files::Orientation as O;
        // 2×1 横条 [红|绿]：像素方位标识旋转/翻转方向
        let mut strip = image::RgbaImage::new(2, 1);
        strip.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        strip.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        let strip = image::DynamicImage::ImageRgba8(strip);
        let red = image::Rgba([255, 0, 0, 255]);
        let green = image::Rgba([0, 255, 0, 255]);

        // EXIF 1：原样
        assert_eq!(
            apply_orientation(strip.clone(), O::NoTransforms).dimensions(),
            (2, 1)
        );
        // EXIF 6：顺时针 90° → 1×2，红（左端）转到顶部
        let r = apply_orientation(strip.clone(), O::Rotate90);
        assert_eq!(r.dimensions(), (1, 2));
        assert_eq!(r.get_pixel(0, 0), red);
        assert_eq!(r.get_pixel(0, 1), green);
        // EXIF 8：顺时针 270° → 1×2，红转到底部
        let r = apply_orientation(strip.clone(), O::Rotate270);
        assert_eq!(r.get_pixel(0, 0), green);
        assert_eq!(r.get_pixel(0, 1), red);
        // EXIF 2：左右镜像 → 红绿互换
        let r = apply_orientation(strip.clone(), O::FlipHorizontal);
        assert_eq!(r.get_pixel(0, 0), green);
        assert_eq!(r.get_pixel(1, 0), red);
        // EXIF 3：180°（高 1 的条上等同左右互换）→ 红绿互换
        let r = apply_orientation(strip.clone(), O::Rotate180);
        assert_eq!(r.get_pixel(0, 0), green);
        // EXIF 4：上下镜像（高 1 无可见变化，验证尺寸与不炸）
        assert_eq!(
            apply_orientation(strip.clone(), O::FlipVertical).dimensions(),
            (2, 1)
        );
        // EXIF 5/7：旋转 + 镜像 → 1×2 转置
        assert_eq!(
            apply_orientation(strip.clone(), O::Rotate90FlipH).dimensions(),
            (1, 2)
        );
        assert_eq!(
            apply_orientation(strip, O::Rotate270FlipH).dimensions(),
            (1, 2)
        );
    }

    #[test]
    fn finish_static_抽取后行为不变() {
        // 共享尾部抽取回归：基础档阈值内 ×factor 预放大；原图资格命中时
        // 顺带带回供 full 升级复用
        let (out, native) =
            finish_static(
                image::DynamicImage::new_rgb8(100, 50),
                4096,
                false,
                2.0,
                &SharpenCfg::OFF,
            )
            .unwrap();
        let Decoded::Static(out) = out else {
            panic!("finish_static 恒返回 Static");
        };
        assert_eq!(out.size, [200, 100]);
        assert!(native.is_some(), "100×50×2≤4096 应命中原图保留资格");
    }
}
