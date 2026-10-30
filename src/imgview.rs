//! 图片纹理直显服务（ARCHITECTURE.md 13.4 第 1-4 步）：
//! 静态图与动图（gif / webp 动画）均不再经 mpv，后台线程 image crate 解码
//! → egui 纹理 → UV 变换绘制；动图按帧延迟定时 repaint 逐帧播放。
//!
//! 数据流：`App::play` 路由判定（[`wants_texture`]）→ [`ImageService::request`]
//! spawn 解码线程（zip/dir 用 VFS 虚拟文件读取；7z/RAR 读已落盘缓存）→
//! 降采样/上采样 → mpsc 回 UI 线程 → [`ImageService::poll`] 上传纹理进 LRU。
//!
//! - 尺寸适配两档（见 [`resample`]）：基础档 = 原图×ppp 预放大（阈值型，
//!   native×ppp ≤ `image_max_dim` 才升，翻页即 100% 密度直出；超限原尺寸
//!   直进）；full 高清档 = 原图×ppp×2（clamp 型，封顶 [`FULL_MAX_DIM`]，
//!   深放大按需二次解码）。病态图（>8192）一律降采样兜底
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

/// 纹理尺寸硬上限（病态图兜底 + full 槽封顶）：防病态大图
/// （30000×30000 像素图等）撑爆显存——8192²×4B=256MB 封顶，
/// 绝大多数"原图"不受影响仍全分辨率。pub 供 UI 层判断升级触发条件
pub const FULL_MAX_DIM: u32 = 8192;

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

/// full 档纹理上传参数：双线性、**无 mipmap**。full 的显示区间从 244%
/// 起（默认阈值 2.0，相对 full 采样比 ≥1.22× 纯放大）——若开 mipmap，
/// 三线性混合会让半分辨率 mip1 占大头，2× Lanczos 的锐度增益被打回
/// base 水平；无 mipmap 的双线性在 ≥0.5× 缩小下仅轻微欠采样，静止图
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

enum ImgMsg {
    Ready {
        key: ImgKey,
        decoded: Decoded,
        /// 文件短名：仅供上传日志标注
        name: String,
    },
    Failed {
        key: ImgKey,
        error: String,
    },
    /// full 槽纹理就绪（原图二次解码 ×ppp×2，见 request_full）
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
    /// 原图纹理单槽：大图放大到 1:1 附近时按需解码不降采样版本无缝换画。
    /// 单槽不进 LRU——原图显存大（8192²×4B=256MB），只随当前显示条目存续
    /// （frame() 切条目即弃）
    full: Option<(ImgKey, egui::TextureHandle)>,
    /// 原图解码中（key + 发起时刻，后者供看门狗超时判定）
    full_pending: Option<(ImgKey, Instant)>,
    /// 原图解码放弃标记（失败/超时/动图）：防止每帧重触发无限重解
    full_failed: Option<ImgKey>,
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
            full: None,
            full_pending: None,
            full_failed: None,
        }
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
        let r = std::thread::Builder::new().name("imgdec".into()).spawn(move || {
            // 解码对畸形文件可能 panic（坏图触发 codec 内部越界等）：
            // 捕获转 Failed。否则线程静默死亡（无 send、无 repaint），
            // pending 永久泄漏 → 该图被幂等检查拦截 → 永久占位死局
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decode_image(&source, max_dim, false, factor, &name, false)
            }));
            match r {
                Ok(Ok(decoded)) => {
                    let _ = tx.send(ImgMsg::Ready {
                        key,
                        decoded,
                        name,
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

    /// 请求解码高清纹理（full 单槽，clamp FULL_MAX_DIM）：深放大按需
    /// 升级，从原图重新解码 Lanczos ×(ppp×2)（150% 缩放屏 ×3）——绝不
    /// 在基础纹理（已 ×ppp）上级联再放大（两级重采样抹锐边）。倍率内
    /// 部按屏幕 DPI 取值，调用方无需传参。
    /// 升级资格由触发方保证：仅 native ≤ image_max_dim 的图（即基础
    /// 纹理走了预放大）值得升级；超大图原尺寸直进 LRU、深放大交给 GPU
    /// 双线性（二次解码秒级 + Lanczos 秒级 + 180MB 级显存，增益 <×1.4
    /// 不值当）。
    /// 就绪后无缝换画
    /// （几何不变：屏幕矩形按基础纹理算，UV 全图映射对两种分辨率通用）。
    /// 幂等：已就绪/解码中/已放弃直接返回；动图不升级（全帧原图显存
    /// 不可控，触发方已用 is_static 前置过滤，单帧动图落到这里按
    /// FullFailed 放弃即可）
    /// sharpen 直传 image_full_sharpen 配置：Lanczos 放大后叠加
    /// unsharp mask 锐化（观感补偿，见 sharpen_full）
    pub fn request_full(
        &mut self,
        key: ImgKey,
        source: ImageSource,
        name: &str,
        sharpen: bool,
    ) {
        if self.has_full(key) || self.is_full_pending(key) || self.is_full_failed(key) {
            return;
        }
        tracing::debug!("发起高清解码 {name}");
        // 高清档倍率 = ppp×2：默认阈值 2.0（244% 档）起由 ×2 密度纹理
        // 接管，采样比 ≥1.22× 近 1:1 无损直到 ×2.5 屏幕倍率；更浅放大
        // base GPU 直放大与之肉眼无差（195% 实拍 A/B），不白付二次解码
        let factor = (self.ctx.pixels_per_point() as f64).max(1.0) * 2.0;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let name = name.to_owned();
        self.full_pending = Some((key, Instant::now()));
        let r = std::thread::Builder::new()
            .name("imgfull".into())
            .spawn(move || {
                // 同 request：畸形文件可能 panic，捕获转 FullFailed 防死局
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_image(&source, FULL_MAX_DIM, true, factor, &name, sharpen)
                }));
                let msg = match r {
                    Ok(Ok(Decoded::Static(img))) => ImgMsg::FullReady { key, img, name },
                    // 单帧动图取唯一帧当静态原图
                    Ok(Ok(Decoded::Anim(mut a))) if a.len() == 1 => {
                        let (img, _) = a.pop().expect("len==1");
                        ImgMsg::FullReady { key, img, name }
                    }
                    Ok(_) | Err(_) => ImgMsg::FullFailed { key },
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
                ImgMsg::Ready {
                    key,
                    decoded,
                    name,
                } => {
                    self.pending.remove(&key);
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
                ImgMsg::FullReady {
                    key,
                    img,
                    name,
                } => {
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
                    }
                }
                ImgMsg::FullFailed { key } => {
                    if self.full_pending.take().is_some_and(|(k, _)| k == key) {
                        tracing::warn!("原图解码失败，保持降采样纹理: {key:?}");
                        self.full_failed = Some(key);
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
        let pos = self.pos(key)?;
        // 原图单槽生命周期跟随显示条目：切条目即弃（旧原图显存大不留驻）；
        // 放弃标记不清——解码失败是确定性的，重试只会再失败一遍
        if self.full.as_ref().is_some_and(|(k, _)| *k != key) {
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

    /// 入缓存并逐出最久未用（超容量时）；逐出跳过 key（刚推入）与 shown
    /// （正在显示）。二者都不堪逐时允许临时超容（break），下次推入再收缩
    fn cache_push(&mut self, key: ImgKey, c: Cached) {
        self.cache.push_front((key, c));
        let shown = self.shown;
        while self.cache.len() > self.cap {
            let Some(victim) = self
                .cache
                .iter()
                .rposition(|(k, _)| *k != key && Some(*k) != shown)
            else {
                break;
            };
            self.cache.remove(victim);
        }
    }
}

/// 后台线程主体：读源 → 解码（gif/webp 逐帧）→ 尺寸适配 → RGBA。
/// full=false 基础档（factor=ppp）/ true 高清档（factor=ppp×2），
/// 两档策略见 resample；factor 由 ImageService 按屏幕 DPI 内部取值；
/// sharpen = 高清档 Lanczos 放大后是否叠加锐化（配置 image_full_sharpen）
fn decode_image(
    source: &ImageSource,
    max_dim: u32,
    full: bool,
    factor: f64,
    name: &str,
    sharpen: bool,
) -> Result<Decoded, String> {
    match source {
        ImageSource::File(path) => {
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
    sharpen: bool,
) -> Result<Decoded, String> {
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
        return Ok(if frames.len() == 1 {
            let (img, _) = frames.into_iter().next().expect("len==1");
            Decoded::Static(img)
        } else {
            Decoded::Anim(frames)
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
            return Ok(if frames.len() == 1 {
                let (img, _) = frames.into_iter().next().expect("len==1");
                Decoded::Static(img)
            } else {
                Decoded::Anim(frames)
            });
        }
        image::DynamicImage::from_decoder(dec).map_err(|e| format!("解码图片失败: {e}"))?
    } else {
        reader
            .decode()
            .map_err(|e| format!("解码图片失败: {e}"))?
    };
    // 两档尺寸适配（策略见 resample）：基础档 LRU 存 native×ppp（翻页
    // 即 100% 密度直出）；高清档 full 槽存 native×ppp×2（深放大用，
    // clamp 8192）。factor 由 ImageService 按屏幕 DPI 定
    let (w, h) = (img.width(), img.height());
    let out = resample(img, max_dim, full, factor);
    // 锐化（配置 image_full_sharpen，仅高清档）：Lanczos 放大的边缘
    // 补偿，见 sharpen_full
    let out = if full && sharpen { sharpen_full(out) } else { out };
    // 尺寸命名日志：native_size 原图 / lru_size 基础纹理 / full_size
    // full 槽纹理（箭头一致 = 未触发重采样，尺寸原样直进）
    tracing::debug!(
        "解码 {fmt:?} [{base}] native_size={w}x{h} {}_size={}x{}{}: {}ms",
        if full { "full" } else { "lru" },
        out.width(),
        out.height(),
        if full && sharpen { " 锐化" } else { "" },
        t.elapsed().as_millis()
    );
    Ok(Decoded::Static(color_image(out)))
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
            max_dim.min(FULL_MAX_DIM),
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
/// - 病态兜底（两档共用）：最长边 > FULL_MAX_DIM(8192) 一律降采样，
///   防 30000×30000 级病图撑爆显存
/// - 基础档（full=false，LRU 缓存纹理）：阈值型——native×factor ≤
///   max_dim 才 Lanczos ×factor 预放大（factor=ppp：翻页即 100% 密度
///   直出，不再"先糊后锐"换画）；×factor 超限的原尺寸直进（超大图
///   纹理密度已足，深放大交给 GPU 双线性，免降采样损失与秒级重采样）
/// - 高清档（full=true，full 单槽）：clamp 型——无条件 Lanczos ×factor
///   封顶 FULL_MAX_DIM（factor=ppp×2；native ≤ image_max_dim 资格由
///   调用方保证，从原图重新解码，绝不在已放大的纹理上级联再放大）
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
    if longest > FULL_MAX_DIM {
        return thumbnail_cap(img, FULL_MAX_DIM);
    }
    if !full {
        // 阈值型：放大后不超 max_dim 才升（×factor 全图等比，最长边
        // 判定即可），超限原尺寸直进
        if (longest as f64 * factor).round() <= max_dim as f64 {
            upscale(img, factor, max_dim)
        } else {
            img
        }
    } else {
        // clamp 型：无条件 ×factor、封顶 FULL_MAX_DIM
        upscale(img, factor, FULL_MAX_DIM)
    }
}

/// Lanczos3 ×factor 等比上采样、最长边封顶 cap（按最长边等比缩，
/// 触顶不畸变——逐边独立 clamp 会让非方形图比例失调；尺寸不变时原样返回）
fn upscale(img: image::DynamicImage, factor: f64, cap: u32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    // 目标最长边：×factor 后封顶 cap（≥1 防零图/极端 factor 归零）
    let target = ((longest as f64 * factor).round() as u32).min(cap).max(1);
    let scale = target as f64 / longest as f64;
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    if nw != w || nh != h {
        img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
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

/// unsharp mask 锐化（仅高清档，配置 image_full_sharpen）：out = px +
/// amount×(px − blur(px))——Lanczos 放大虽是"最诚实重建"，但插值本质
/// 会把边缘坡度摊平（1px 边缘摊成 factor px 缓坡），锐化把坡度重新变
/// 陡，深放大观感明显改善（信息量不变，纯观感补偿；强对比边缘可能有
/// 轻微光晕，配置可关）。sigma 1.0 ≈ 2px 邻域 / amount 0.6 温和档；
/// 仅 RGB，alpha 直通。O(n) 后台线程执行（千万像素级 ~几十 ms）
fn sharpen_full(img: image::DynamicImage) -> image::DynamicImage {
    const SIGMA: f32 = 1.0;
    const AMOUNT: f32 = 0.6;
    let mut rgba = img.to_rgba8();
    let blurred = image::imageops::blur(&rgba, SIGMA);
    for (p, b) in rgba.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let v = p[c] as f32 + AMOUNT * (p[c] as f32 - b[c] as f32);
            p[c] = v.clamp(0.0, 255.0) as u8;
        }
    }
    image::DynamicImage::ImageRgba8(rgba)
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
        assert_eq!(zoom_pan(egui::Vec2::ZERO, 2.0, 4.0, egui::Vec2::ZERO), egui::Vec2::ZERO);
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
        let p = clamp_pan(egui::vec2(10_000.0, -10_000.0), egui::vec2(4000.0, 3000.0), vp);
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
        assert_eq!((out.width(), out.height()), (3000, 2000), "×1.5 超 4096 原图直进");
        // ppp=1.0 屏（factor=1）：不无谓重采样
        let out = resample(image::DynamicImage::new_rgb8(800, 600), 4096, false, 1.0);
        assert_eq!((out.width(), out.height()), (800, 600));
    }

    #[test]
    fn resample_高清档clamp放大() {
        // clamp 型：无条件 ×factor、按最长边等比封顶 FULL_MAX_DIM
        // （factor=ppp×2；native ≤ image_max_dim 资格由触发方保证）
        let out = resample(image::DynamicImage::new_rgb8(1920, 1080), 8192, true, 2.0);
        assert_eq!((out.width(), out.height()), (3840, 2160));
        let out = resample(image::DynamicImage::new_rgb8(4096, 3072), 8192, true, 2.0);
        assert_eq!((out.width(), out.height()), (8192, 6144), "×2 恰顶格");
        let out = resample(image::DynamicImage::new_rgb8(2000, 1500), 8192, true, 4.0);
        assert_eq!((out.width(), out.height()), (8000, 6000), "×4 未触顶整体放大");
        let out = resample(image::DynamicImage::new_rgb8(4096, 1000), 8192, true, 4.0);
        // 最长边 16384→8192 等比缩（scale=2），短边随比例 1000→2000
        //（逐边独立 clamp 会畸变成 8192×4000）
        assert_eq!((out.width(), out.height()), (8192, 2000));
    }

    #[test]
    fn resample_病态图8192兜底() {
        // 两档共用兜底：>8192 病图等比降到 8192（快速 box 采样 + mipmap）
        let out = resample(image::DynamicImage::new_rgb8(9000, 7000), 4096, false, 1.0);
        assert_eq!((out.width(), out.height()), (8192, 6372)); // 7000×8192/9000=6371.6→6372
        let out = resample(image::DynamicImage::new_rgb8(10000, 8000), 8192, true, 3.0);
        assert_eq!((out.width(), out.height()), (8192, 6554), "高清档同样先兜底"); // 8000×0.8192=6553.6
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
            16, 16, image::Luma([128]),
        ));
        let out = sharpen_full(flat).to_rgba8();
        assert!(out.pixels().all(|p| p[0] == 128));
        // 垂直阶跃：左 64 右 192（取中灰区间防 0/255 饱和掩盖效果），
        // 边缘内侧低侧压暗、高侧提亮 = 坡度变陡
        let mut img = image::GrayImage::new(16, 8);
        for row in img.rows_mut() {
            for (x, p) in row.enumerate() {
                *p = image::Luma([if x < 8 { 64 } else { 192 }]);
            }
        }
        let out = sharpen_full(image::DynamicImage::ImageLuma8(img)).to_rgba8();
        assert!(out.get_pixel(7, 4)[0] < 64, "边缘低侧应压暗");
        assert!(out.get_pixel(8, 4)[0] > 192, "边缘高侧应提亮");
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
        let decode = |sharpen| {
            let out =
                decode_image(&ImageSource::File(p.clone()), 8192, true, 1.0, "s.png", sharpen)
                    .unwrap();
            let Decoded::Static(img) = out else {
                panic!("PNG 应走静态路径");
            };
            img
        };
        let (off, on) = (decode(false), decode(true));
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
        let out = decode_image(&ImageSource::File(p.clone()), 4096, false, 2.0, "t.png", false).unwrap();
        std::fs::remove_file(&p).ok();
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
        let out = decode_image(&ImageSource::File(p.clone()), 4096, false, 2.0, "t.png", false).unwrap();
        std::fs::remove_file(&p).ok();
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
        let out = decode_image(&ImageSource::File(p.clone()), 4096, false, 2.0, "t.png", false).unwrap();
        std::fs::remove_file(&p).ok();
        assert!(matches!(out, Decoded::Static(_)), "单帧动画应归静态（免时钟开销）");
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
        let out = decode_image(&ImageSource::File(p.clone()), 4096, false, 2.0, "t.png", false).unwrap();
        std::fs::remove_file(&p).ok();
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
        // 同款逐帧机制；动图不预放大，帧为画布原尺寸
        let bytes = include_bytes!("testdata/anim2.webp");
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim2.webp");
        std::fs::write(&p, bytes).unwrap();
        let out = decode_image(&ImageSource::File(p.clone()), 4096, false, 2.0, "anim2.webp", false).unwrap();
        std::fs::remove_file(&p).ok();
        let Decoded::Anim(frames) = out else {
            panic!("动画 webp 应走逐帧路径（Decoded::Anim）");
        };
        assert_eq!(frames.len(), 2, "两个 ANMF 帧都应解出");
        assert_eq!(frames[0].0.size, [20, 14], "动图帧为画布原尺寸（不预放大）");
        assert_eq!(frames[0].1, Duration::from_millis(100));
        // 帧序与颜色：首帧红、次帧蓝（lavfi 纯色 + VP8L 无损，通道优势明显）
        let (r, g, b, _) = frames[0].0.pixels[0].to_tuple();
        assert!(r > 200 && g < 60 && b < 60, "首帧应为红色，实为 ({r},{g},{b})");
        let (r, g, b, _) = frames[1].0.pixels[0].to_tuple();
        assert!(b > 200 && r < 60 && g < 60, "次帧应为蓝色，实为 ({r},{g},{b})");
    }

    #[test]
    fn decode_高清路径中图两倍预放大() {
        // 800×600：基础档阈值内升 ×factor（见 decode_file_从临时文件解码），
        // 高清档 clamp 型同样 ×2（800×2=1600 ≤8192 未触顶）——深放大
        // 区间 Lanczos 细节优于 GPU 双线性
        let png = png_bytes(800, 600);
        let dir = std::env::temp_dir().join(format!("archview_imgtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("full.png");
        std::fs::write(&p, &png).unwrap();
        let out = decode_image(&ImageSource::File(p.clone()), 8192, true, 2.0, "t.png", false).unwrap();
        std::fs::remove_file(&p).ok();
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
        let out = decode_image(&ImageSource::File(p.clone()), 8192, true, 1.5, "t.png", false).unwrap();
        std::fs::remove_file(&p).ok();
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
        let out = decode_image(&ImageSource::File(p.clone()), 4096, false, 2.0, "t.png", false).unwrap();
        std::fs::remove_file(&p).ok();
        let Decoded::Static(img) = out else {
            panic!("PNG 应走静态路径");
        };
        assert_eq!(img.size, [1200, 800]);
    }

    /// 直接塞一帧进服务缓存（跳过后台线程，同步可控）
    fn feed(svc: &mut ImageService, key: ImgKey, decoded: Decoded, name: &str) {
        let _ = svc.tx.send(ImgMsg::Ready {
            key,
            decoded,
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
        let frame = |c: u8| {
            egui::ColorImage::new([4, 4], vec![egui::Color32::from_gray(c); 16])
        };
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
            Decoded::Static(egui::ColorImage::new([4, 4], vec![egui::Color32::WHITE; 16])),
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
}
