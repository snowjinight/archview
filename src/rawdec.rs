//! raw 裸数据媒体层（与 imgview.rs 平级）：裸像素 dump 的参数解析、格式
//! 校验、Bayer 排列探测与软解帧提取。两类消费者：
//! - mpv 播放路径（ui/extract.rs）：RawLoad → rawvideo demuxer 拼参装载；
//! - 对比视图软解路径（ui/compare.rs）：RawFrameService 软解帧 → egui 纹理。
//!
//! 分层定位：媒体能力模块，不含 UI 组合逻辑。像素格式名单与文件名分辨率
//! 兜底见 crate::config；mpv 侧流层 bit_shift 见 player::stream_cb::ShiftReader
//! （与本模块 shift_u16_le 同语义，各自独立实现）。

use crate::config::Config;
use crate::imgview::{Histogram, ImageSource, ImgKey};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

// ---------------------------------------------------------------------------
// 装载参数与预设校验
// ---------------------------------------------------------------------------

/// raw dump 命中预设且校验通过后的装载参数：`opts()` 生成 loadfile 第四
/// 参数（rawvideo demuxer 拼参）；`bit_shift` = 流层每 u16 采样左移位数
/// （0 = 不位移，仅快路径 zvf:// 生效）；`auto_bayer` = pixfmt 为伪格式
/// bayer_auto16le，装载前须经 resolve_auto_bayer 读首帧探测排列并替换
#[derive(Debug, PartialEq)]
pub(crate) struct RawLoad {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) pixfmt: String,
    pub(crate) fps: f64,
    pub(crate) bit_shift: u32,
    pub(crate) auto_bayer: bool,
}

impl RawLoad {
    /// loadfile 第四参数（拼参格式与 player e2e 测试同源）
    pub(crate) fn opts(&self) -> String {
        format!(
            "demuxer=rawvideo,demuxer-rawvideo-w={},demuxer-rawvideo-h={},\
             demuxer-rawvideo-mp-format={},demuxer-rawvideo-fps={}",
            self.w, self.h, self.pixfmt, self.fps
        )
    }
}

/// raw dump 预设解析 + 不读文件的整除校验（方案 §4.5）：
/// Ok(Some(load)) = 命中预设且校验通过（loadfile 第四参数 + 流层
/// bit_shift）；Ok(None) = 非裸数据（标准容器，无需选项）；
/// Err(msg) = 配置错误（pixfmt 不在名单 / 宽高缺失且文件名无分辨率 /
/// 大小不整除 / bit_shift 越界或非 le 格式），调用方 set_err 并放弃装载。
/// 校验必须在 loadfile 之前：rawvideo demuxer 对尾部零头字节不报错，
/// 画面会错位且难排查。
///
/// 宽高兜底（文件名分辨率）：预设 width/height 为 0，或预设宽高整除
/// 校验失败时，解析文件名内嵌 WxH（如 fpga_in_s0_1920x1080_00_1.raw）
/// 逐候选重走整除校验，首个通过者胜出；候选序 = 预设宽高（若有效）
/// 在前、文件名按出现顺序在后
pub(crate) fn raw_demux_opts(
    cfg: &Config,
    name: &str,
    size: Option<u64>,
) -> Result<Option<RawLoad>, String> {
    let Some(d) = cfg.raw_demux(name) else {
        return Ok(None);
    };
    // 1) 格式名前置校验（名单见 README「raw 格式表」；bayer_auto16le
    //    为伪格式=16bit 容器，排列装载时自动探测）
    let Some(bits) = crate::config::raw_pixfmt_bits(&d.pixfmt) else {
        return Err(format!(
            "raw 预设像素格式 '{}' 不被支持（预设 match='{}'）。\n\
             常用：yuv420p / nv12 / yuv422p / yuyv422 / gray / gray16le / bayer_rggb8 / bayer_rggb16le / bayer_auto16le…\n\
             完整名单见 README「raw 格式表」（195 项，含全部 yuv/rgb/gray/bayer 家族）。",
            d.pixfmt, d.r#match
        ));
    };
    let auto_bayer = d.pixfmt.eq_ignore_ascii_case("bayer_auto16le");
    // 2) bit_shift 合法域：0~8；>0 须 le 结尾（按 LE u16 采样
    //    移位，be/1 字节格式位移会破坏数据）
    if d.bit_shift > 8
        || d.bit_shift > 0 && !d.pixfmt.to_ascii_lowercase().ends_with("le")
    {
        return Err(format!(
            "raw 预设 match='{}' 的 bit_shift={} 不合法（pixfmt={}）：\
             取值 0~8；>0 时 pixfmt 须为 le 结尾的 16bit 容器格式\
             （如 bayer_bggr16le；12bit 低对齐数据填 4、10bit 填 6）。",
            d.r#match, d.bit_shift, d.pixfmt
        ));
    }
    // 3) 宽高候选：预设非 0 优先，文件名内嵌 WxH 兜底（预设配错分辨率
    //    时整除校验会失败，文件名分辨率是 dump 工具写进名字的可靠参数）
    let mut candidates: Vec<(u32, u32)> = Vec::new();
    if d.width > 0 && d.height > 0 {
        candidates.push((d.width, d.height));
    }
    for c in crate::config::parse_filename_dims(name) {
        if !candidates.contains(&c) {
            candidates.push(c);
        }
    }
    if candidates.is_empty() {
        return Err(format!(
            "raw 预设 match='{}' 的 width/height 未填（pixfmt={}），且文件名\
             无内嵌分辨率：宽高无法确定。\n\
             请在预设填 width/height，或让文件名携带 WxH（如 xxx_1920x1080_00.raw）。",
            d.r#match, d.pixfmt
        ));
    }
    // 4) 帧整除校验（位数为基，兼容 yuv410p=9bit 等非整数字节格式），
    //    逐候选取首个通过者
    for &(w, h) in &candidates {
        let frame_bits = w as u64 * h as u64 * bits as u64;
        if let Some(size) = size {
            if size * 8 % frame_bits != 0 {
                continue;
            }
        }
        return Ok(Some(RawLoad {
            w,
            h,
            pixfmt: d.pixfmt.clone(),
            fps: d.fps,
            bit_shift: d.bit_shift,
            auto_bayer,
        }));
    }
    let tried = candidates
        .iter()
        .map(|(w, h)| format!("{w}x{h}"))
        .collect::<Vec<_>>()
        .join("、");
    Err(format!(
        "文件大小 {} B 与任一候选分辨率（{tried}）的 {} 单帧大小都不整除，无法切帧。\n\
         请核对预设 match='{}' 的 width/height/pixfmt 是否与该文件匹配。",
        size.unwrap_or(0), d.pixfmt, d.r#match
    ))
}

// ---------------------------------------------------------------------------
// Bayer 排列自动探测（伪格式 bayer_auto16le 专用）
// ---------------------------------------------------------------------------

/// 读首帧前若干行按 LE u16 解析（12bit 低对齐数据用容器原值即可，统计
/// 不受影响），两级启发式：
/// ① 主/反对角逐对均值差——G 同色强相关、R-B 跨色弱相关，差值小的对角线
///    即 G-G 对角：反对角 → {rggb, bggr}，主对角 → {gbrg, grbg}；
/// ② 组内两非 G 位置采样均值，大者判为 R（自然图像/彩条下 R>B 统计成立；
///    相对差 <1% 视为不可分辨）。置信不足返回 None，调用方按 RGGB 兜底。
/// None 另见：帧过小（<8x4）/ 读取失败
pub(crate) fn detect_bayer_layout(read: &mut impl std::io::Read, w: u32, h: u32) -> Option<&'static str> {
    use std::io::Read as _;
    const MAX_ROWS: usize = 128; // 采样行数上限（1920 宽 ≈ 512KB，zip 头部解压开销可忽略）
    if w < 8 || h < 4 {
        return None;
    }
    let stride = w as usize * 2;
    let mut buf = Vec::with_capacity(MAX_ROWS * stride);
    read.take(MAX_ROWS as u64 * stride as u64)
        .read_to_end(&mut buf)
        .ok()?;
    let rows = buf.len() / stride;
    let rows = rows - rows % 2; // 2x2 块对齐
    if rows < 4 {
        return None;
    }
    let w = w as usize;
    let px = |r: usize, c: usize| -> u16 {
        u16::from_le_bytes([buf[r * stride + c * 2], buf[r * stride + c * 2 + 1]])
    };
    // 偶对齐 2x2 块内统计：d_main=|(0,0)-(1,1)|，d_anti=|(0,1)-(1,0)|；
    // 同时累加四个位置的均值（组内 R/B 判别用）
    let (mut d_main, mut d_anti) = (0u64, 0u64);
    let (mut s00, mut s01, mut s10, mut s11) = (0u64, 0u64, 0u64, 0u64);
    let mut n = 0u64;
    for br in (0..rows - 1).step_by(2) {
        for bc in (0..w - 1).step_by(2) {
            let (p00, p01) = (px(br, bc) as u64, px(br, bc + 1) as u64);
            let (p10, p11) = (px(br + 1, bc) as u64, px(br + 1, bc + 1) as u64);
            d_main += p00.abs_diff(p11);
            d_anti += p01.abs_diff(p10);
            s00 += p00;
            s01 += p01;
            s10 += p10;
            s11 += p11;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    // ① G-G 对角（需明显差距，噪声/纯灰数据不可靠 → 交由调用方兜底）
    let (gg_is_main, d_small, d_large) = if d_main < d_anti {
        (true, d_main, d_anti)
    } else {
        (false, d_anti, d_main)
    };
    if d_large == 0 || d_small * 100 > d_large * 95 {
        return None;
    }
    // ② 组内 R/B 均值判别（R>B 统计；相对差 <1% 或均值近零不可分辨）
    let mean = |s: u64| s as f64 / n as f64;
    let rggb_or_bggr = |m00: f64, m11: f64| -> Option<&'static str> {
        if (m00 + m11).abs() < 2.0 || (m00 - m11).abs() * 100.0 < (m00 + m11) * 0.5 {
            return None;
        }
        Some(if m00 > m11 { "bayer_rggb16le" } else { "bayer_bggr16le" })
    };
    let gbrg_or_grbg = |m01: f64, m10: f64| -> Option<&'static str> {
        if (m01 + m10).abs() < 2.0 || (m01 - m10).abs() * 100.0 < (m01 + m10) * 0.5 {
            return None;
        }
        Some(if m01 > m10 { "bayer_grbg16le" } else { "bayer_gbrg16le" })
    };
    if gg_is_main {
        // G 在主对角 (0,0)/(1,1)，R/B 在 (0,1)/(1,0)
        gbrg_or_grbg(mean(s01), mean(s10))
    } else {
        // G 在反对角 (0,1)/(1,0)，R/B 在 (0,0)/(1,1)
        rggb_or_bggr(mean(s00), mean(s11))
    }
}

/// 伪格式 bayer_auto16le 的装载前解析：读首帧探测排列并替换 pixfmt；
/// 探测不可靠（数据不足/纯灰）按 RGGB 兜底并记日志
pub(crate) fn resolve_auto_bayer(r: &mut RawLoad, read: &mut impl std::io::Read) {
    if !r.auto_bayer {
        return;
    }
    r.auto_bayer = false;
    let probed = detect_bayer_layout(read, r.w, r.h).unwrap_or("bayer_rggb16le");
    tracing::info!("bayer 排列探测: {} ({}x{})", probed, r.w, r.h);
    r.pixfmt = probed.to_string();
}

// ---------------------------------------------------------------------------
// 像素转换器（纯函数；与 mpv 路径 bit_shift 语义逐位一致）
// ---------------------------------------------------------------------------

/// u16 LE 采样流按序配对左移（与 stream_cb::ShiftReader 同语义：12bit 低
/// 对齐数据左移后高 8 位即有效位；wrapping_shl 高位溢出丢弃即期望语义）
pub(crate) fn shift_u16_le(data: &mut [u8], shift: u32) {
    if shift == 0 {
        return;
    }
    for pair in data.chunks_exact_mut(2) {
        let v = u16::from_le_bytes([pair[0], pair[1]]).wrapping_shl(shift);
        pair[..].copy_from_slice(&v.to_le_bytes());
    }
}

/// nv12 → RGB8（BT.601）。布局：Y 平面 W×H + UV 交错平面（W/2×H/2 对，
/// 行宽 W 字节）
pub(crate) fn nv12_to_rgb(w: usize, h: usize, data: &[u8]) -> Result<Vec<u8>, String> {
    let yp = w * h;
    if data.len() < yp + yp / 2 {
        return Err(format!("nv12 数据不足：需 {a} 字节，实得 {b}", a = yp * 3 / 2, b = data.len()));
    }
    let (y, uv) = data.split_at(yp);
    let mut out = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        let yrow = &y[row * w..row * w + w];
        let uvrow = &uv[(row / 2) * w..(row / 2) * w + w];
        for col in 0..w {
            let yy = yrow[col] as f32;
            let u = uvrow[col & !1] as f32 - 128.0;
            let v = uvrow[(col & !1) + 1] as f32 - 128.0;
            let r = (yy + 1.402 * v).clamp(0.0, 255.0) as u8;
            let g = (yy - 0.344136 * u - 0.714136 * v).clamp(0.0, 255.0) as u8;
            let b = (yy + 1.772 * u).clamp(0.0, 255.0) as u8;
            out.extend_from_slice(&[r, g, b]);
        }
    }
    Ok(out)
}

/// gray 8bit → RGB8
pub(crate) fn gray8_to_rgb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 3);
    for &v in data {
        out.extend_from_slice(&[v, v, v]);
    }
    out
}

/// gray16le（可选 bit_shift）→ RGB8（取高 8 位）
pub(crate) fn gray16le_to_rgb(data: &[u8], shift: u32) -> Vec<u8> {
    data.chunks_exact(2)
        .map(|c| ((u16::from_le_bytes([c[0], c[1]]).wrapping_shl(shift)) >> 8) as u8)
        .flat_map(|v| [v, v, v])
        .collect()
}

/// Bayer 相位 → 通道（0=R, 1=G, 2=B）。layout = [左上, 右上, 左下, 右下]
fn bayer_channel(layout: &[u8; 4], x: usize, y: usize) -> usize {
    match (y % 2, x % 2) {
        (0, 0) => layout[0] as usize,
        (0, 1) => layout[1] as usize,
        (1, 0) => layout[2] as usize,
        _ => layout[3] as usize,
    }
}

/// 排列名（rggb/bggr/grbg/gbrg）→ 2×2 相位表
fn bayer_layout_of(name: &str) -> Option<[u8; 4]> {
    match name {
        "rggb" => Some([0, 1, 1, 2]),
        "bggr" => Some([2, 1, 1, 0]),
        "grbg" => Some([1, 0, 2, 1]),
        "gbrg" => Some([1, 2, 0, 1]),
        _ => None,
    }
}

/// 每相位 × 每通道的插值偏移表：dx,dy ∈ [-2,2] 中该相位处为目标通道的
/// 位置，权重 = 1/距离²（双线性近邻加权；边缘 clamp 由取样时处理）
fn bayer_offsets(layout: &[u8; 4]) -> [Vec<Vec<(isize, isize, f32)>>; 4] {
    let mut out: [Vec<Vec<(isize, isize, f32)>>; 4] =
        std::array::from_fn(|_| vec![Vec::new(), Vec::new(), Vec::new()]);
    for pr in 0..2usize {
        for pc in 0..2usize {
            let phase = pr * 2 + pc;
            for c in 0..3usize {
                let mut v = Vec::new();
                for dy in -2isize..=2 {
                    for dx in -2isize..=2 {
                        let d2 = dx * dx + dy * dy;
                        if d2 == 0 {
                            continue;
                        }
                        let ch = bayer_channel(
                            layout,
                            (pc as isize + dx).rem_euclid(2) as usize,
                            (pr as isize + dy).rem_euclid(2) as usize,
                        );
                        if ch == c {
                            v.push((dx, dy, 1.0 / d2 as f32));
                        }
                    }
                }
                out[phase][c] = v;
            }
        }
    }
    out
}

/// bayer 16bit LE（可选 bit_shift）→ RGB8。双线性 debayer：自有通道直采，
/// 其余通道按相位偏移表加权平均（对比场景要求各栏转换一致，不做色彩
/// 科学级精度）
pub(crate) fn bayer16le_to_rgb(
    w: usize,
    h: usize,
    data: &[u8],
    layout: &[u8; 4],
    shift: u32,
) -> Result<Vec<u8>, String> {
    if data.len() < w * h * 2 {
        return Err(format!(
            "bayer 数据不足：需 {need} 字节，实得 {got}",
            need = w * h * 2,
            got = data.len()
        ));
    }
    let samples: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]).wrapping_shl(shift))
        .collect();
    let offs = bayer_offsets(layout);
    let at = |x: isize, y: isize| -> u16 {
        let x = x.clamp(0, w as isize - 1) as usize;
        let y = y.clamp(0, h as isize - 1) as usize;
        samples[y * w + x]
    };
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let phase = (y % 2) * 2 + x % 2;
            for c in 0..3usize {
                let own = bayer_channel(layout, x, y) == c;
                let v = if own {
                    samples[y * w + x]
                } else {
                    let (mut sum, mut wsum) = (0f32, 0f32);
                    for &(dx, dy, wt) in &offs[phase][c] {
                        sum += at(x as isize + dx, y as isize + dy) as f32 * wt;
                        wsum += wt;
                    }
                    if wsum > 0.0 {
                        (sum / wsum) as u16
                    } else {
                        // 病态小图：该相位无该通道邻点 → 灰度兜底
                        samples[y * w + x]
                    }
                };
                let b = (v >> 8) as u8;
                out.push(b);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 帧参数与软解服务（每请求 spawn 线程，mpsc 回 UI 上传纹理）
// ---------------------------------------------------------------------------

/// 软解支持的格式判定（预设命中即可见可对比；冷门 pixfmt 入栏显示占位
/// 错误，播放路径不受影响）
fn soft_decodable(pixfmt: &str, auto_bayer: bool) -> bool {
    let p = pixfmt.to_ascii_lowercase();
    auto_bayer
        || p == "gray"
        || p == "gray16le"
        || p == "nv12"
        || (p.starts_with("bayer_") && p.ends_with("16le"))
}

/// raw dump 栏的帧参数（raw_demux_opts 校验通过后派生；播放路径不受影响）
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawFrameInfo {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) pixfmt: String,
    pub(crate) fps: f64,
    pub(crate) bit_shift: u32,
    pub(crate) auto_bayer: bool,
    pub(crate) frame_bytes: u64,
    pub(crate) frame_count: usize,
}

impl RawFrameInfo {
    fn to_raw_load(&self) -> RawLoad {
        RawLoad {
            w: self.w,
            h: self.h,
            pixfmt: self.pixfmt.clone(),
            fps: self.fps,
            bit_shift: self.bit_shift,
            auto_bayer: self.auto_bayer,
        }
    }
}

/// raw_demux_opts 校验结果 → 帧参数（含帧数 = size/frame_bytes，整除校验
/// 已由 raw_demux_opts 前置保证）
pub(crate) fn raw_frame_info(rl: &RawLoad, size: u64) -> Result<RawFrameInfo, String> {
    if !soft_decodable(&rl.pixfmt, rl.auto_bayer) {
        return Err(format!(
            "暂不支持软解该格式：{}（支持 nv12 / gray / gray16le / bayer*_16le；\
             播放不受影响）",
            rl.pixfmt
        ));
    }
    let bits = crate::config::raw_pixfmt_bits(&rl.pixfmt)
        .ok_or_else(|| format!("raw 格式位宽未知: {}", rl.pixfmt))?;
    let frame_bytes = rl.w as u64 * rl.h as u64 * bits as u64 / 8;
    if frame_bytes == 0 {
        return Err("raw 帧字节为 0（宽高为 0？）".into());
    }
    Ok(RawFrameInfo {
        w: rl.w,
        h: rl.h,
        pixfmt: rl.pixfmt.clone(),
        fps: rl.fps,
        bit_shift: rl.bit_shift,
        auto_bayer: rl.auto_bayer,
        frame_bytes,
        frame_count: (size / frame_bytes).max(1) as usize,
    })
}

pub(crate) struct RawFrameTex {
    pub(crate) idx: usize,
    pub(crate) tex: egui::TextureHandle,
}

struct RawFrameDone {
    key: ImgKey,
    idx: usize,
    res: Result<(egui::ColorImage, [u32; 2]), String>,
}

/// raw 帧解码服务（与 ImageService 并列，同一数据源不同消费者）
pub(crate) struct RawFrameService {
    tx: Sender<RawFrameDone>,
    rx: Receiver<RawFrameDone>,
    /// (key, 帧号) → 发起时刻（防重复 spawn）
    pending: HashMap<(ImgKey, usize), Instant>,
    /// 每栏已就绪帧（保留上一帧直到新帧就绪，切换无闪烁）
    frames: HashMap<ImgKey, RawFrameTex>,
    /// 确定性失败（格式不支持/数据坏）：不再重试
    errs: HashMap<ImgKey, String>,
    ctx: egui::Context,
}

impl RawFrameService {
    pub(crate) fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            pending: HashMap::new(),
            frames: HashMap::new(),
            errs: HashMap::new(),
            ctx,
        }
    }

    /// 请求解码第 idx 帧（幂等；idx 按栏帧数钳制）
    pub(crate) fn request(
        &mut self,
        key: ImgKey,
        source: &ImageSource,
        info: &RawFrameInfo,
        frame_idx: usize,
    ) {
        let fi = frame_idx.min(info.frame_count.saturating_sub(1));
        if self.frames.get(&key).is_some_and(|t| t.idx == fi)
            || self.pending.contains_key(&(key, fi))
            || self.errs.contains_key(&key)
        {
            return;
        }
        self.pending.insert((key, fi), Instant::now());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (source, info) = (source.clone(), info.clone());
        std::thread::Builder::new()
            .name("rawdec".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_raw_frame(&source, &info, fi)
                }))
                .map_err(|_| "raw 帧解码线程 panic".to_string())
                .and_then(|r| r);
                let _ = tx.send(RawFrameDone { key, idx: fi, res });
                ctx.request_repaint();
            })
            .ok();
    }

    /// UI 线程每帧排空结果并上传纹理
    pub(crate) fn poll(&mut self) {
        let mut repaint = false;
        while let Ok(m) = self.rx.try_recv() {
            self.pending.remove(&(m.key, m.idx));
            match m.res {
                Ok((img, _native)) => {
                    let tex = self.ctx.load_texture(
                        format!("raw-{}-{}-f{}", m.key.0, m.key.1, m.idx),
                        img,
                        egui::TextureOptions::LINEAR,
                    );
                    self.frames.insert(m.key, RawFrameTex { idx: m.idx, tex });
                }
                Err(e) => {
                    tracing::warn!("raw 帧解码失败 [{:?} f{}]: {e}", m.key, m.idx);
                    self.errs.insert(m.key, e);
                }
            }
            repaint = true;
        }
        if repaint {
            self.ctx.request_repaint();
        }
    }

    /// 已就绪帧（不问帧号是否最新——新帧在途时保留旧帧绘制，无闪烁）
    pub(crate) fn frame(&self, key: ImgKey) -> Option<&RawFrameTex> {
        self.frames.get(&key)
    }

    pub(crate) fn err(&self, key: ImgKey) -> Option<&String> {
        self.errs.get(&key)
    }

    /// 请求在途判定（预留：等待文案区分用）
    #[allow(dead_code)]
    pub(crate) fn is_pending(&self, key: ImgKey) -> bool {
        self.pending.keys().any(|(k, _)| *k == key)
    }

    /// 退出对比/滑动后清理不在当前窗口的栏
    pub(crate) fn retain(&mut self, keys: &[ImgKey]) {
        self.frames.retain(|k, _| keys.contains(k));
        self.errs.retain(|k, _| keys.contains(k));
        self.pending.retain(|(k, _), _| keys.contains(k));
    }
}

struct HistDone {
    key: ImgKey,
    res: Result<Histogram, String>,
}

/// raw 直方图软解服务（Ctrl+H 单图直方图的 raw 侧入口）：单图显示走
/// mpv dump 播放（GPU 侧，无 CPU 像素），直方图需要像素 → 按需软解
/// 第 0 帧（复用 decode_raw_frame：同一位移/转换链，直方图与屏幕显示
/// 逐位一致）。事件驱动旁路消费者：Ctrl+H 才发起、后台线程、不干预
/// mpv 显示路径；只算直方图不传纹理
pub(crate) struct HistService {
    tx: Sender<HistDone>,
    rx: Receiver<HistDone>,
    /// key → 发起时刻（防重复 spawn）
    pending: HashMap<ImgKey, Instant>,
    /// 已就绪结果（Ok=直方图 / Err=确定性失败文案，不重试）
    done: HashMap<ImgKey, Result<Histogram, String>>,
    ctx: egui::Context,
}

impl HistService {
    pub(crate) fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            pending: HashMap::new(),
            done: HashMap::new(),
            ctx,
        }
    }

    /// 确保当前 key 的直方图在途（幂等）。预设未命中（标准容器/视频）与
    /// 配置错误（Err）直接落 done——UI 覆盖层显示状态文案，不发线程
    pub(crate) fn request(&mut self, key: ImgKey, source: &ImageSource, name: &str, size: u64, cfg: &Config) {
        if self.pending.contains_key(&key) || self.done.contains_key(&key) {
            return;
        }
        let rl = match raw_demux_opts(cfg, name, Some(size)) {
            Ok(Some(rl)) => rl,
            Ok(None) => {
                self.done.insert(key, Err("非 raw 裸数据（标准视频不支持直方图）".into()));
                return;
            }
            Err(e) => {
                self.done.insert(key, Err(format!("raw 预设错误: {e}")));
                return;
            }
        };
        let info = match raw_frame_info(&rl, size) {
            Ok(i) => i,
            Err(e) => {
                self.done.insert(key, Err(format!("raw 帧参数错误: {e}")));
                return;
            }
        };
        self.pending.insert(key, Instant::now());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (source, name) = (source.clone(), name.to_owned());
        let r = std::thread::Builder::new()
            .name("rawhist".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // 第 0 帧：直方图语义是"这张 dump 长什么样"；多帧序列
                    // 逐帧统计成本线性涨，首帧足够代表（覆盖层标注帧号）
                    decode_raw_frame(&source, &info, 0).map(|(img, _)| Histogram::compute(&img))
                }))
                .map_err(|_| "raw 直方图线程 panic".to_string())
                .and_then(|r| r);
                tracing::debug!("raw 直方图[{name}]: {}", res.is_ok());
                let _ = tx.send(HistDone { key, res });
                ctx.request_repaint();
            });
        if r.is_err() {
            self.pending.remove(&key);
            self.done.insert(key, Err("直方图线程创建失败".into()));
        }
    }

    /// UI 线程每帧排空结果
    pub(crate) fn poll(&mut self) {
        let mut repaint = false;
        while let Ok(m) = self.rx.try_recv() {
            self.pending.remove(&m.key);
            self.done.insert(m.key, m.res);
            repaint = true;
        }
        if repaint {
            self.ctx.request_repaint();
        }
    }

    /// Ok(Some(hist)) = 就绪；Ok(None) = 在途（软解中）；Err = 失败文案
    pub(crate) fn histogram(&self, key: ImgKey) -> Option<&Result<Histogram, String>> {
        self.done.get(&key)
    }

    /// 切条目后清理非当前 key（直方图按 key 即用即弃，不驻留）
    pub(crate) fn retain(&mut self, keys: &[ImgKey]) {
        self.pending.retain(|k, _| keys.contains(k));
        self.done.retain(|k, _| keys.contains(k));
    }
}

fn decode_raw_frame(
    source: &ImageSource,
    info: &RawFrameInfo,
    frame_idx: usize,
) -> Result<(egui::ColorImage, [u32; 2]), String> {
    use std::io::{Read, Seek, SeekFrom};
    enum Reader {
        V(Box<dyn crate::vfs::VirtualFile>),
        F(std::fs::File),
    }
    impl Read for Reader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self {
                Reader::V(v) => v.read(buf),
                Reader::F(f) => f.read(buf),
            }
        }
    }
    impl Seek for Reader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            match self {
                Reader::V(v) => v.seek(pos),
                Reader::F(f) => f.seek(pos),
            }
        }
    }
    let mut r = match source {
        ImageSource::Stream(vfs, e) => {
            Reader::V(vfs.open_entry(e).map_err(|er| format!("打开条目失败: {er}"))?)
        }
        ImageSource::File(p) => Reader::F(
            std::fs::File::open(p).map_err(|er| format!("打开缓存文件失败: {er}"))?,
        ),
    };
    // 伪格式 bayer_auto16le：读首帧探测排列（先探测后转换，encode 侧
    // "拒绝伪格式"约束不涉及本路径）
    let mut rl = info.to_raw_load();
    if rl.auto_bayer {
        r.seek(SeekFrom::Start(0))
            .map_err(|e| format!("seek 失败: {e}"))?;
        resolve_auto_bayer(&mut rl, &mut r);
    }
    let pixfmt = rl.pixfmt.to_ascii_lowercase();
    // 帧字节寻址：第 k 帧 = k×frame_bytes（raw 的 seek 是精确乘法）
    let off = frame_idx as u64 * info.frame_bytes;
    r.seek(SeekFrom::Start(off))
        .map_err(|e| format!("seek 第 {frame_idx} 帧失败: {e}"))?;
    let mut data = vec![0u8; info.frame_bytes as usize];
    r.read_exact(&mut data)
        .map_err(|e| format!("读帧数据失败: {e}"))?;
    if info.bit_shift > 0 {
        shift_u16_le(&mut data, info.bit_shift);
    }
    let (w, h) = (info.w as usize, info.h as usize);
    let rgb = match pixfmt.as_str() {
        "gray" => Ok(gray8_to_rgb(&data)),
        "gray16le" => Ok(gray16le_to_rgb(&data, 0)),
        "nv12" => nv12_to_rgb(w, h, &data),
        p if p.starts_with("bayer_") && p.ends_with("16le") => {
            let name = &p["bayer_".len()..p.len() - "16le".len()];
            bayer_layout_of(name)
                .ok_or_else(|| format!("未知 bayer 排列: {name}"))
                .and_then(|layout| bayer16le_to_rgb(w, h, &data, &layout, 0))
        }
        _ => Err(format!(
            "暂不支持软解该格式：{}（支持 nv12 / gray / gray16le / bayer*_16le）",
            rl.pixfmt
        )),
    }?;
    let pixels: Vec<egui::Color32> = rgb
        .chunks_exact(3)
        .map(|c| egui::Color32::from_rgb(c[0], c[1], c[2]))
        .collect();
    Ok((
        egui::ColorImage::new([w as usize, h as usize], pixels),
        [info.w, info.h],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hist_service_非raw与预设错误直落结果() {
        // 非线程路径即答：标准容器（预设未命中）→"非 raw"；预设配置错
        // → 预设错误文案。两者不发软解线程，UI 覆盖层直接显示状态
        let ctx = egui::Context::default();
        let mut svc = HistService::new(ctx);
        let key: ImgKey = (0, 0);
        let src = ImageSource::File(std::path::PathBuf::from("nonexistent.bin"));
        let cfg = crate::config::Config::default();

        // 标准视频名：无 raw 预设命中 → Ok(None)
        svc.request(key, &src, "movie.mp4", 12345, &cfg);
        let msg = match svc.histogram(key) {
            Some(Err(m)) => m.clone(),
            other => panic!("应直落 Err，实际 {other:?}"),
        };
        assert!(msg.contains("非 raw"), "文案应指明非 raw: {msg}");

        // 预设命中但 pixfmt 不在名单 → Err 带预设错误
        let mut bad = crate::config::Config::default();
        bad.raw.demux = vec![crate::config::RawDemux {
            r#match: "*.bin".into(),
            pixfmt: "notafmt".into(),
            width: 64,
            height: 48,
            fps: 10.0,
            bit_shift: 0,
        }];
        let key2: ImgKey = (0, 1);
        svc.request(key2, &src, "sensor.bin", 12345, &bad);
        let msg = match svc.histogram(key2) {
            Some(Err(m)) => m.clone(),
            other => panic!("应直落 Err，实际 {other:?}"),
        };
        assert!(msg.contains("raw 预设错误"), "文案应指明预设错误: {msg}");

        // retain 清理非当前 key
        svc.retain(&[key2]);
        assert_eq!(svc.histogram(key), None, "retain 应清掉旧 key");
        assert!(svc.histogram(key2).is_some(), "当前 key 保留");
    }

    #[test]
    fn shift_u16_le_12bit低对齐左移4() {
        // 12bit 低对齐样本 0x0ABC 左移 4 = 0xABC0（高 8 位即有效位 0xAB）
        let mut d = 0x0ABCu16.to_le_bytes().to_vec();
        shift_u16_le(&mut d, 4);
        assert_eq!(d, 0xABC0u16.to_le_bytes().to_vec());
        let v = u16::from_le_bytes([d[0], d[1]]);
        assert_eq!(v >> 8, 0xAB);
    }

    #[test]
    fn gray8_roundtrip() {
        let d = vec![0u8, 128, 255];
        let rgb = gray8_to_rgb(&d);
        assert_eq!(rgb, vec![0, 0, 0, 128, 128, 128, 255, 255, 255]);
    }

    #[test]
    fn nv12_中性灰零色偏() {
        // Y=128, U=V=128 → RGB ≈ (128,128,128)
        let w = 2usize;
        let h = 2usize;
        let mut d = vec![128u8; w * h]; // Y
        d.extend(vec![128u8; w * h / 2]); // UV
        let rgb = nv12_to_rgb(w, h, &d).unwrap();
        assert_eq!(rgb.len(), w * h * 3);
        for px in rgb.chunks_exact(3) {
            assert!((px[0] as i32 - 128).abs() <= 1 && (px[1] as i32 - 128).abs() <= 1);
        }
    }

    #[test]
    fn nv12_色度偏移方向正确() {
        // U=0（Cb=-128）、V=0（Cr=-128）：BT.601 下 G 大幅抬升、R/B 压到 0
        let w = 2usize;
        let h = 2usize;
        let mut d = vec![128u8; w * h];
        d.extend(vec![0u8; w * h / 2]);
        let rgb = nv12_to_rgb(w, h, &d).unwrap();
        let px = &rgb[..3];
        assert!(px[1] > px[0] && px[1] > px[2], "G 应最大: {px:?}");
    }

    #[test]
    fn gray16le_取高8位() {
        let d = 0xABCDu16.to_le_bytes().to_vec();
        let rgb = gray16le_to_rgb(&d, 0);
        assert_eq!(rgb[0], 0xAB);
    }

    #[test]
    fn bayer_rggb_自有通道直采() {
        // 4x4 RGGB：纯红场（R=0xFFFF，其余 0）→ 输出 R 满值
        let w = 4usize;
        let h = 4usize;
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let ch = bayer_channel(&[0, 1, 1, 2], x, y);
                let v = if ch == 0 { 0xFFFFu16 } else { 0u16 };
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
        let rgb = bayer16le_to_rgb(w, h, &data, &[0, 1, 1, 2], 0).unwrap();
        // 中心像素 (1,1) 是 G 位置：R 由邻近 R 平均（非零），B 为 0 邻域
        let px = &rgb[(1 * w + 1) * 3..(1 * w + 1) * 3 + 3];
        assert!(px[0] > 0, "R 插值应非零: {px:?}");
        // (0,0) 是 R 位置：R 直采满值
        let px00 = &rgb[..3];
        assert_eq!(px00[0], 255);
    }

    #[test]
    fn bayer_四排列相位表互异() {
        let layouts: [&[u8; 4]; 4] = [&[0, 1, 1, 2], &[2, 1, 1, 0], &[1, 0, 2, 1], &[1, 2, 0, 1]];
        for l in layouts {
            // 每相位四通道表覆盖 RGB 且 G 恰两个
            for pr in 0..2 {
                for pc in 0..2 {
                    let mut cnt = [0usize; 3];
                    for dy in 0..2 {
                        for dx in 0..2 {
                            cnt[bayer_channel(l, pc + dx, pr + dy)] += 1;
                        }
                    }
                    assert_eq!(cnt[1], 2, "G 应恰 2 个");
                    assert_eq!(cnt[0], 1);
                    assert_eq!(cnt[2], 1);
                }
            }
        }
    }

    #[test]
    fn raw_frame_info_帧数与软解判定() {
        let rl = RawLoad {
            w: 1920,
            h: 1080,
            pixfmt: "nv12".into(),
            fps: 25.0,
            bit_shift: 0,
            auto_bayer: false,
        };
        let size = 1920 * 1080 * 3 / 2 * 100;
        let info = raw_frame_info(&rl, size).unwrap();
        assert_eq!(info.frame_bytes, (1920 * 1080 * 3 / 2) as u64);
        assert_eq!(info.frame_count, 100);
        assert!(!info.to_raw_load().auto_bayer);
        // yuv420p 不在软解支持名单 → RawErr
        let rl2 = RawLoad { pixfmt: "yuv420p".into(), ..rl };
        assert!(raw_frame_info(&rl2, size).is_err());
    }
}
