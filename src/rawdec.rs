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

/// 宽高候选链（文件名分辨率兜底，方案 §13.6）：预设宽高非 0 在前，
/// 文件名内嵌 WxH 候选按出现顺序去重在后——播放 raw_demux_opts 与编码
/// resolve_encode_format 两条消费链共享的候选口径，参数解析语义必须对齐
pub(crate) fn dims_candidates(pw: u32, ph: u32, name: &str) -> Vec<(u32, u32)> {
    let mut candidates: Vec<(u32, u32)> = Vec::new();
    if pw > 0 && ph > 0 {
        candidates.push((pw, ph));
    }
    for c in crate::config::parse_filename_dims(name) {
        if !candidates.contains(&c) {
            candidates.push(c);
        }
    }
    candidates
}

/// 帧整除校验（位数为基，兼容 yuv410p=9bit 等非整数字节格式）：
/// size*8 是否为 w*h*bits 整数倍；size 未知（None）视为通过
pub(crate) fn frame_divisible(size: Option<u64>, w: u32, h: u32, bits: u32) -> bool {
    match size {
        Some(s) => s * 8 % (u64::from(w) * u64::from(h) * u64::from(bits)) == 0,
        None => true,
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
    if d.bit_shift > 8 || d.bit_shift > 0 && !d.pixfmt.to_ascii_lowercase().ends_with("le") {
        return Err(format!(
            "raw 预设 match='{}' 的 bit_shift={} 不合法（pixfmt={}）：\
             取值 0~8；>0 时 pixfmt 须为 le 结尾的 16bit 容器格式\
             （如 bayer_bggr16le；12bit 低对齐数据填 4、10bit 填 6）。",
            d.r#match, d.bit_shift, d.pixfmt
        ));
    }
    // 3) 宽高候选：预设非 0 优先，文件名内嵌 WxH 兜底（预设配错分辨率
    //    时整除校验会失败，文件名分辨率是 dump 工具写进名字的可靠参数）
    let candidates = dims_candidates(d.width, d.height, name);
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
        if !frame_divisible(size, w, h, bits) {
            continue;
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
        size.unwrap_or(0),
        d.pixfmt,
        d.r#match
    ))
}

// ---------------------------------------------------------------------------
// Bayer 排列自动探测（伪格式 bayer_auto16le 专用）
// ---------------------------------------------------------------------------

/// 读首帧前若干行按 LE u16 解析。所有门限均为**无量纲比值**——有效位深
/// （8/10/12/14bit）与高/低对齐只决定数值尺度，不改比值统计，因此探测
/// 不需要知道位深。按 2x2 块累加四个位置的采样均值（n≈数千块：噪声被
/// 平均掉、纹理不改变均值——逐像素差分类方案在细纹理/高噪声下会把
/// "跨通道差异"淹没在"像素间涨落"里而误判不可靠，故弃用）：
/// ① G 对角判别：G 两位置同通道 → 均值差只剩统计涨落；非 G 对角是 R/B
///    跨通道 → 均值差显著。均值差小的对角线即 G 对角：反对角 → {rggb,
///    bggr}，主对角 → {gbrg, grbg}。判别须过两重门限：
///    - 结构性：较大对角均值差 > 四位置总均值 × 0.5%（排除纯灰/平场）；
///    - 分离度：较小对角均值差 < 较大者 × 40%（同通道均值差须远小于
///      跨通道差，防两对角亮度凑巧接近的题材误判）。
/// ② 组内 R/B 均值判别：大者判为 R（自然图像/彩条下 R>B 统计成立；
///    相对差 <1% 视为不可分辨）。
/// 置信不足返回 None，调用方按 RGGB 兜底。None 另见：帧过小（<8x4）/ 读取失败
pub(crate) fn detect_bayer_layout(
    read: &mut impl std::io::Read,
    w: u32,
    h: u32,
) -> Option<&'static str> {
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
    // 2x2 块内按位置累加（均值统计，对噪声/纹理稳健）
    let (mut s00, mut s01, mut s10, mut s11) = (0u64, 0u64, 0u64, 0u64);
    let mut n = 0u64;
    for br in (0..rows - 1).step_by(2) {
        for bc in (0..w - 1).step_by(2) {
            s00 += px(br, bc) as u64;
            s01 += px(br, bc + 1) as u64;
            s10 += px(br + 1, bc) as u64;
            s11 += px(br + 1, bc + 1) as u64;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    let mean = |s: u64| s as f64 / n as f64;
    let (m00, m01, m10, m11) = (mean(s00), mean(s01), mean(s10), mean(s11));
    let level = (m00 + m01 + m10 + m11) / 4.0;
    let dm = (m00 - m11).abs(); // 主对角对内均值差
    let da = (m01 - m10).abs(); // 反对角对内均值差
    let (small, large) = if dm < da { (dm, da) } else { (da, dm) };
    // ① 结构性 + 分离度（无量纲门限，位深/对齐无关）
    if large <= level * 0.005 || small * 100.0 > large * 40.0 {
        return None;
    }
    // ② R/B 判别（相对差 <1% 不可分辨；对内均值和为 0 即全黑对 → 不可分辨）
    let rb = |a: f64, b: f64| -> Option<bool> {
        if a + b <= 0.0 || (a - b).abs() * 100.0 < (a + b) * 0.5 {
            return None;
        }
        Some(a > b)
    };
    if dm > da {
        // G 在反对角 (0,1)/(1,0)，R/B 在 (0,0)/(1,1)；亮者判 R
        rb(m00, m11).map(|bright00| {
            if bright00 {
                "bayer_rggb16le"
            } else {
                "bayer_bggr16le"
            }
        })
    } else {
        // G 在主对角 (0,0)/(1,1)，R/B 在 (0,1)/(1,0)；亮者判 R
        rb(m01, m10).map(|bright01| {
            if bright01 {
                "bayer_grbg16le"
            } else {
                "bayer_gbrg16le"
            }
        })
    }
}

/// 伪格式 bayer_auto16le 的装载前解析：读首帧探测排列并替换 pixfmt；
/// 探测不可靠（数据不足/纯灰）按 RGGB 兜底并记日志
pub(crate) fn resolve_auto_bayer(r: &mut RawLoad, read: &mut impl std::io::Read) {
    if !r.auto_bayer {
        return;
    }
    r.auto_bayer = false;
    // 兜底必须显式记 WARN（与真探测区分）——此前两条路同打一条 INFO
    // "bayer 排列探测"，用户把静默兜底误读成确认成功
    let probed = match detect_bayer_layout(read, r.w, r.h) {
        Some(name) => {
            tracing::info!("bayer 排列探测: {} ({}x{})", name, r.w, r.h);
            name
        }
        None => {
            tracing::warn!(
                "bayer 排列探测置信不足（帧过小/数据不足/纯灰数据），\
                 按 bayer_rggb16le 兜底 ({}x{})",
                r.w,
                r.h
            );
            "bayer_rggb16le"
        }
    };
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
        return Err(format!(
            "nv12 数据不足：需 {a} 字节，实得 {b}",
            a = yp * 3 / 2,
            b = data.len()
        ));
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

/// yuv420p（I420）→ RGB8（BT.601）。布局：Y/U/V 三个独立平面，U/V 各
/// (W/2)×(H/2)（4:2:0 色度半采样；与 nv12 的差别只在 UV 平面组织——
/// nv12 交错在同一行，yuv420p 两个独立平面）
pub(crate) fn yuv420p_to_rgb(w: usize, h: usize, data: &[u8]) -> Result<Vec<u8>, String> {
    let yp = w * h;
    let cp = (w / 2).max(1) * (h / 2).max(1);
    if data.len() < yp + cp * 2 {
        return Err(format!(
            "yuv420p 数据不足：需 {a} 字节，实得 {b}",
            a = yp + cp * 2,
            b = data.len()
        ));
    }
    let (y, rest) = data.split_at(yp);
    let (u, v) = rest.split_at(cp);
    let cw = (w / 2).max(1);
    let mut out = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        let yrow = &y[row * w..row * w + w];
        let urow = &u[(row / 2) * cw..(row / 2) * cw + cw];
        let vrow = &v[(row / 2) * cw..(row / 2) * cw + cw];
        for col in 0..w {
            let yy = yrow[col] as f32;
            let u = urow[col / 2] as f32 - 128.0;
            let v = vrow[col / 2] as f32 - 128.0;
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
/// 错误，播放路径不受影响）。
/// **新增格式约束**：此处新增支持时，必须同步扩展 `probe_raw_pixel` 的
/// 格式分派（探针集 = 软解集，两处 match 逐条对应），否则新格式栏可看
/// 不可探针。
fn soft_decodable(pixfmt: &str, auto_bayer: bool) -> bool {
    let p = pixfmt.to_ascii_lowercase();
    auto_bayer
        || p == "gray"
        || p == "gray16le"
        || p == "nv12"
        || p == "yuv420p"
        || p == "rgb24"
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
            "暂不支持软解该格式：{}（支持 nv12 / yuv420p / rgb24 / gray / \
             gray16le / bayer*_16le；播放不受影响）",
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
    /// 本帧显示对比度（per-file 步数 n，0=原值）：缓存随 contrast 陈旧判定——
    /// W/S 调对比度后同帧按新 contrast 重发重解（raw 无熵编码重解几十 ms）
    pub(crate) contrast: i32,
    pub(crate) tex: egui::TextureHandle,
    /// 本帧直方图（软解线程顺手算，与纹理同源同帧）：对比模式 Ctrl+H
    /// 直方图覆盖层用，显示帧与直方图永远一致（保留旧帧时直方图同旧）
    pub(crate) hist: Histogram,
}

struct RawFrameDone {
    key: ImgKey,
    idx: usize,
    contrast: i32,
    res: Result<(egui::ColorImage, [u32; 2], Histogram), String>,
}

/// raw 探针读数（各内建格式各自的"真实值"，全部为**文件存储原值**——
/// 未做 bit_shift 位移、未转换色彩空间；显示 RGB 是转换产物，探针要的是
/// 传感器/编码前的原始采样值）
#[derive(Clone, Debug)]
pub(crate) enum RawProbe {
    /// gray/gray16le：单点亮度存储值 + 有效位深（gray=8；gray16le 位移后
    /// = 16-bit_shift，如 12bit 数据）
    Gray { v: u16, bits: u32 },
    /// nv12/yuv420p：该点 Y + 覆盖它的 2×2 块共享的 U/V（4:2:0 色度半采样，
    /// UV 按 (x/2, y/2) 取——两种 4:2:0 的差别只在平面组织，取样语义相同）
    Nv12 { y: u8, u: u8, v: u8 },
    /// rgb24：该点 RGB 三分量存储原值（逐像素 3 字节，R,G,B 字节序）
    Rgb { r: u8, g: u8, b: u8 },
    /// bayer*_16le：采样点对齐 2×2 块的 4 个存储原值（CFA 排列位序）+ 排列
    /// 名 + 有效位深
    Bayer {
        layout: String,
        block: [[u16; 2]; 2],
        bits: u32,
    },
}

struct RawProbeDone {
    key: ImgKey,
    idx: usize,
    uv: egui::Vec2,
    res: Result<RawProbe, String>,
}

enum RawMsg {
    Frame(RawFrameDone),
    Probe(RawProbeDone),
}

/// raw 帧解码服务（与 ImageService 并列，同一数据源不同消费者）
pub(crate) struct RawFrameService {
    tx: Sender<RawMsg>,
    rx: Receiver<RawMsg>,
    /// (key, 帧号, contrast) → 发起时刻（防重复 spawn；contrast 入键——调
    /// 对比度后同帧按新 contrast 重发，旧请求在途不阻塞新请求）
    pending: HashMap<(ImgKey, usize, i32), Instant>,
    /// 每栏已就绪帧（保留上一帧直到新帧就绪，切换无闪烁）
    frames: HashMap<ImgKey, RawFrameTex>,
    /// 确定性失败（格式不支持/数据坏）：不再重试
    errs: HashMap<ImgKey, String>,
    /// 像素探针取数状态（key → 最近一次请求；res None = 在途）。不做缓存
    /// 语义——帧游标或取样点变化由 UI 重发（request_probe 幂等），读源
    /// 直接按字节偏移取所需几个字节，无解码成本
    probes: HashMap<ImgKey, ProbeEntry>,
    ctx: egui::Context,
}

/// 探针取数条目：请求时的 (帧号, 取样点 uv)；res None = 在途
pub(crate) struct ProbeEntry {
    pub(crate) idx: usize,
    pub(crate) uv: egui::Vec2,
    pub(crate) res: Option<Result<RawProbe, String>>,
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
            probes: HashMap::new(),
            ctx,
        }
    }

    /// 请求解码第 idx 帧（幂等；idx 按栏帧数钳制）。contrast = per-file 显示
    /// 对比度（步数 n）：worker 解码后 LUT，直方图在 LUT 后像素上统计
    /// （显示帧与直方图一致，与 imgview poll 期语义同源）
    pub(crate) fn request(
        &mut self,
        key: ImgKey,
        source: &ImageSource,
        info: &RawFrameInfo,
        frame_idx: usize,
        contrast: i32,
    ) {
        let fi = frame_idx.min(info.frame_count.saturating_sub(1));
        if self
            .frames
            .get(&key)
            .is_some_and(|t| t.idx == fi && t.contrast == contrast)
            || self.pending.contains_key(&(key, fi, contrast))
            || self.errs.contains_key(&key)
        {
            return;
        }
        self.pending.insert((key, fi, contrast), Instant::now());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (source, info) = (source.clone(), info.clone());
        std::thread::Builder::new()
            .name("rawdec".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // 直方图顺手算（O(像素)，被解码耗时掩盖；与 imgview 解码
                    // worker 同款先例）：contrast LUT 先落显示像素、直方图统计
                    // LUT 后像素，与纹理同源
                    decode_raw_frame(&source, &info, fi).map(|(mut img, native)| {
                        if let Some(l) = crate::imgview::contrast_lut(contrast) {
                            crate::imgview::apply_contrast(&mut img, &l);
                        }
                        let hist = Histogram::compute(&img);
                        (img, native, hist)
                    })
                }))
                .map_err(|_| "raw 帧解码线程 panic".to_string())
                .and_then(|r| r);
                let _ = tx.send(RawMsg::Frame(RawFrameDone {
                    key,
                    idx: fi,
                    contrast,
                    res,
                }));
                ctx.request_repaint();
            })
            .ok();
    }

    /// 像素探针取数（Alt+点击 raw 栏）：异步读源解析单点/2×2 块存储原值。
    /// 幂等——同 key 同帧同点不重发；帧游标或取样点变化由 UI 重发覆盖
    pub(crate) fn request_probe(
        &mut self,
        key: ImgKey,
        source: &ImageSource,
        info: &RawFrameInfo,
        frame_idx: usize,
        uv: egui::Vec2,
    ) {
        let fi = frame_idx.min(info.frame_count.saturating_sub(1));
        if self
            .probes
            .get(&key)
            .is_some_and(|e| e.idx == fi && e.uv == uv)
        {
            return;
        }
        self.probes.insert(
            key,
            ProbeEntry {
                idx: fi,
                uv,
                res: None,
            },
        );
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (source, info) = (source.clone(), info.clone());
        std::thread::Builder::new()
            .name("rawprobe".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    probe_raw_pixel(&source, &info, fi, uv)
                }))
                .map_err(|_| "raw 探针线程 panic".to_string())
                .and_then(|r| r);
                let _ = tx.send(RawMsg::Probe(RawProbeDone {
                    key,
                    idx: fi,
                    uv,
                    res,
                }));
                ctx.request_repaint();
            })
            .ok();
    }

    /// 探针取数状态（None = 无请求；res None = 在途）
    pub(crate) fn probe(&self, key: ImgKey) -> Option<&ProbeEntry> {
        self.probes.get(&key)
    }

    /// UI 线程每帧排空结果并上传纹理
    pub(crate) fn poll(&mut self) {
        let mut repaint = false;
        while let Ok(m) = self.rx.try_recv() {
            match m {
                RawMsg::Frame(m) => {
                    self.pending.remove(&(m.key, m.idx, m.contrast));
                    match m.res {
                        Ok((img, _native, hist)) => {
                            let tex = self.ctx.load_texture(
                                format!("raw-{}-{}-f{}", m.key.0, m.key.1, m.idx),
                                img,
                                egui::TextureOptions::LINEAR,
                            );
                            self.frames.insert(
                                m.key,
                                RawFrameTex {
                                    idx: m.idx,
                                    contrast: m.contrast,
                                    tex,
                                    hist,
                                },
                            );
                        }
                        Err(e) => {
                            tracing::warn!("raw 帧解码失败 [{:?} f{}]: {e}", m.key, m.idx);
                            self.errs.insert(m.key, e);
                        }
                    }
                }
                RawMsg::Probe(m) => {
                    // 只收当前请求的 (帧号, 点)：UI 已换请求后迟到的结果丢弃
                    if let Some(e) = self.probes.get_mut(&m.key) {
                        if e.idx == m.idx && e.uv == m.uv {
                            e.res = Some(m.res);
                        }
                    }
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
        self.pending.keys().any(|(k, _, _)| *k == key)
    }

    /// 退出对比/滑动后清理不在当前窗口的栏
    pub(crate) fn retain(&mut self, keys: &[ImgKey]) {
        self.frames.retain(|k, _| keys.contains(k));
        self.errs.retain(|k, _| keys.contains(k));
        self.pending.retain(|(k, _, _), _| keys.contains(k));
        self.probes.retain(|k, _| keys.contains(k));
    }
}

struct HistDone {
    key: ImgKey,
    contrast: i32,
    res: Result<Histogram, String>,
}

/// 直方图结果条目（contrast 记录在案：W/S 调对比度后同 key 按 contrast 陈旧判定重算）
struct HistEntry {
    contrast: i32,
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
    done: HashMap<ImgKey, HistEntry>,
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

    /// 确保当前 key 的直方图在途（幂等；contrast 变化视为陈旧自动重算）。
    /// 预设未命中（标准容器/视频）与配置错误（Err）直接落 done——UI 覆盖层
    /// 显示状态文案，不发线程
    pub(crate) fn request(
        &mut self,
        key: ImgKey,
        source: &ImageSource,
        name: &str,
        size: u64,
        cfg: &Config,
        contrast: i32,
    ) {
        if self.pending.contains_key(&key)
            || self
                .done
                .get(&key)
                .is_some_and(|e| e.contrast == contrast && e.res.is_ok())
        {
            return;
        }
        let rl = match raw_demux_opts(cfg, name, Some(size)) {
            Ok(Some(rl)) => rl,
            Ok(None) => {
                self.done.insert(
                    key,
                    HistEntry {
                        contrast,
                        res: Err("非 raw 裸数据（标准视频不支持直方图）".into()),
                    },
                );
                return;
            }
            Err(e) => {
                self.done.insert(
                    key,
                    HistEntry {
                        contrast,
                        res: Err(format!("raw 预设错误: {e}")),
                    },
                );
                return;
            }
        };
        let info = match raw_frame_info(&rl, size) {
            Ok(i) => i,
            Err(e) => {
                self.done.insert(
                    key,
                    HistEntry {
                        contrast,
                        res: Err(format!("raw 帧参数错误: {e}")),
                    },
                );
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
                    // 逐帧统计成本线性涨，首帧足够代表（覆盖层标注帧号）。
                    // contrast LUT 先落显示像素再统计（显示帧与直方图一致）
                    decode_raw_frame(&source, &info, 0).map(|(mut img, _)| {
                        if let Some(l) = crate::imgview::contrast_lut(contrast) {
                            crate::imgview::apply_contrast(&mut img, &l);
                        }
                        Histogram::compute(&img)
                    })
                }))
                .map_err(|_| "raw 直方图线程 panic".to_string())
                .and_then(|r| r);
                tracing::debug!("raw 直方图[{name}]: {}", res.is_ok());
                let _ = tx.send(HistDone { key, contrast, res });
                ctx.request_repaint();
            });
        if r.is_err() {
            self.pending.remove(&key);
            self.done.insert(
                key,
                HistEntry {
                    contrast,
                    res: Err("直方图线程创建失败".into()),
                },
            );
        }
    }

    /// UI 线程每帧排空结果
    pub(crate) fn poll(&mut self) {
        let mut repaint = false;
        while let Ok(m) = self.rx.try_recv() {
            self.pending.remove(&m.key);
            self.done.insert(
                m.key,
                HistEntry {
                    contrast: m.contrast,
                    res: m.res,
                },
            );
            repaint = true;
        }
        if repaint {
            self.ctx.request_repaint();
        }
    }

    /// Ok(Some(hist)) = 就绪；Ok(None) = 在途（软解中）；Err = 失败文案
    pub(crate) fn histogram(&self, key: ImgKey) -> Option<&Result<Histogram, String>> {
        self.done.get(&key).map(|e| &e.res)
    }

    /// 切条目后清理非当前 key（直方图按 key 即用即弃，不驻留）
    pub(crate) fn retain(&mut self, keys: &[ImgKey]) {
        self.pending.retain(|k, _| keys.contains(k));
        self.done.retain(|k, _| keys.contains(k));
    }
}

/// 帧读取器：VFS 条目（压缩包流式）或缓存文件（非流式已落盘）统一
/// Read+Seek 抽象（decode_raw_frame / probe_raw_pixel 共用）
enum FrameReader {
    V(Box<dyn crate::vfs::VirtualFile>),
    F(std::fs::File),
}

impl std::io::Read for FrameReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            FrameReader::V(v) => v.read(buf),
            FrameReader::F(f) => f.read(buf),
        }
    }
}

impl std::io::Seek for FrameReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            FrameReader::V(v) => v.seek(pos),
            FrameReader::F(f) => f.seek(pos),
        }
    }
}

fn open_frame_reader(source: &ImageSource) -> Result<FrameReader, String> {
    match source {
        ImageSource::Stream(vfs, e) => Ok(FrameReader::V(
            vfs.open_entry(e)
                .map_err(|er| format!("打开条目失败: {er}"))?,
        )),
        ImageSource::File(p) => Ok(FrameReader::F(
            std::fs::File::open(p).map_err(|er| format!("打开缓存文件失败: {er}"))?,
        )),
    }
}

fn decode_raw_frame(
    source: &ImageSource,
    info: &RawFrameInfo,
    frame_idx: usize,
) -> Result<(egui::ColorImage, [u32; 2]), String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut r = open_frame_reader(source)?;
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
        "yuv420p" => yuv420p_to_rgb(w, h, &data),
        // rgb24 = 逐像素 R,G,B 字节序，与 egui::Color32::from_rgb 同序——直通
        "rgb24" => Ok(data),
        p if p.starts_with("bayer_") && p.ends_with("16le") => {
            let name = &p["bayer_".len()..p.len() - "16le".len()];
            bayer_layout_of(name)
                .ok_or_else(|| format!("未知 bayer 排列: {name}"))
                .and_then(|layout| bayer16le_to_rgb(w, h, &data, &layout, 0))
        }
        _ => Err(format!(
            "暂不支持软解该格式：{}（支持 nv12 / yuv420p / rgb24 / gray / \
             gray16le / bayer*_16le）",
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

/// 像素探针读数（Alt+点击 raw 栏）：按格式解析采样点的**存储原值**——
/// 不做 bit_shift 位移、不转换色彩空间（显示 RGB 是转换产物，探针要原始
/// 采样值）。按字节偏移直读所需几个字节（auto_bayer 需先读首帧探测排列），
/// 无整帧读取/转换成本：
/// - gray/gray16le：单点亮度值；
/// - nv12/yuv420p：该点 Y + 覆盖 2×2 块共享的 U/V（4:2:0 半采样，按
///   (x/2, y/2) 取；差别只在平面组织，取样语义相同）；
/// - rgb24：该点 RGB 三分量；
/// - bayer*_16le：采样点对齐 2×2 块的 4 个 u16 原值（CFA 排列位序）。
///
/// **新增格式约束**：与 `soft_decodable`/`decode_raw_frame` 的格式分派
/// 逐条对应（探针集 = 软解集），软解新增支持时此处必须同步加分支。
fn probe_raw_pixel(
    source: &ImageSource,
    info: &RawFrameInfo,
    frame_idx: usize,
    uv: egui::Vec2,
) -> Result<RawProbe, String> {
    use std::io::{Read, Seek, SeekFrom};
    let (w, h) = (info.w as usize, info.h as usize);
    let x = ((uv.x * w as f32) as usize).clamp(0, w.saturating_sub(1));
    let y = ((uv.y * h as f32) as usize).clamp(0, h.saturating_sub(1));
    let mut r = open_frame_reader(source)?;
    let mut rl = info.to_raw_load();
    if rl.auto_bayer {
        r.seek(SeekFrom::Start(0))
            .map_err(|e| format!("seek 失败: {e}"))?;
        resolve_auto_bayer(&mut rl, &mut r);
    }
    let pixfmt = rl.pixfmt.to_ascii_lowercase();
    // 帧内按字节偏移直读（帧寻址同 decode：第 k 帧 = k×frame_bytes）
    let frame_off = frame_idx as u64 * info.frame_bytes;
    let mut read_at = |off: u64, len: usize| -> Result<Vec<u8>, String> {
        r.seek(SeekFrom::Start(frame_off + off))
            .map_err(|e| format!("seek 失败: {e}"))?;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)
            .map_err(|e| format!("读探针数据失败: {e}"))?;
        Ok(buf)
    };
    let bits = 16u32.saturating_sub(info.bit_shift);
    match pixfmt.as_str() {
        "gray" => {
            let b = read_at((y * w + x) as u64, 1)?;
            Ok(RawProbe::Gray {
                v: u16::from(b[0]),
                bits: 8,
            })
        }
        "gray16le" => {
            let b = read_at(((y * w + x) * 2) as u64, 2)?;
            Ok(RawProbe::Gray {
                v: u16::from_le_bytes([b[0], b[1]]),
                bits,
            })
        }
        "nv12" => {
            let yp = w * h;
            let cw = (w / 2).max(1);
            let ch = (h / 2).max(1);
            let (cx, cy) = ((x / 2).min(cw - 1), (y / 2).min(ch - 1));
            let yb = read_at((y * w + x) as u64, 1)?;
            let ub = read_at((yp + cy * cw + cx) as u64, 1)?;
            let vb = read_at((yp + yp / 4 + cy * cw + cx) as u64, 1)?;
            Ok(RawProbe::Nv12 {
                y: yb[0],
                u: ub[0],
                v: vb[0],
            })
        }
        // yuv420p = nv12 的取样语义 + 独立 UV 平面（V 平面在 U 平面之后）
        "yuv420p" => {
            let yp = w * h;
            let cw = (w / 2).max(1);
            let ch = (h / 2).max(1);
            let cp = cw * ch;
            let (cx, cy) = ((x / 2).min(cw - 1), (y / 2).min(ch - 1));
            let yb = read_at((y * w + x) as u64, 1)?;
            let ub = read_at((yp + cy * cw + cx) as u64, 1)?;
            let vb = read_at((yp + cp + cy * cw + cx) as u64, 1)?;
            Ok(RawProbe::Nv12 {
                y: yb[0],
                u: ub[0],
                v: vb[0],
            })
        }
        // rgb24 = 逐像素 R,G,B 字节序，单点直读 3 字节
        "rgb24" => {
            let b = read_at(((y * w + x) * 3) as u64, 3)?;
            Ok(RawProbe::Rgb {
                r: b[0],
                g: b[1],
                b: b[2],
            })
        }
        p if p.starts_with("bayer_") && p.ends_with("16le") => {
            let name = &p["bayer_".len()..p.len() - "16le".len()];
            bayer_layout_of(name).ok_or_else(|| format!("未知 bayer 排列: {name}"))?;
            let (bx, by) = (x & !1, y & !1);
            let row0 = read_at(((by * w + bx) * 2) as u64, 4)?;
            let row1 = read_at((((by + 1) * w + bx) * 2) as u64, 4)?;
            let at = |b: &[u8], i: usize| u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
            Ok(RawProbe::Bayer {
                layout: name.to_ascii_uppercase(),
                block: [[at(&row0, 0), at(&row0, 1)], [at(&row1, 0), at(&row1, 1)]],
                bits,
            })
        }
        _ => Err(format!(
            "暂不支持探针该格式：{}（支持 nv12 / gray / gray16le / bayer*_16le）",
            rl.pixfmt
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 均值统计版探测对噪声/纹理稳健（旧逐像素差分版在这两类数据上会误判
    /// 不可靠）+ 门限无量纲（位深/对齐缩放不变）
    #[test]
    fn detect_bayer_layout_均值统计对噪声纹理稳健() {
        let (w, h) = (64u32, 64u32);
        let mut st = 0x2545F4914F6CDD1Du64;
        let mut rnd = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        let ch = |r: usize, c: usize| match (r % 2, c % 2) {
            (0, 0) => 3000u64,
            (0, 1) | (1, 0) => 2000,
            _ => 1000,
        };
        let pack = |vals: Vec<u64>| -> Vec<u8> {
            vals.iter()
                .flat_map(|&v| (v.min(65535) as u16).to_le_bytes())
                .collect()
        };

        // ① 高噪声：通道基值 + 每像素 ±900 均匀噪声（逐像素差分被噪声
        //    淹没，均值差不受影响 → 仍判 rggb）
        let buf = pack(
            (0..h as usize)
                .flat_map(|r| (0..w as usize).map(move |c| (r, c)))
                .map(|(r, c)| ch(r, c) + rnd() % 1800)
                .collect(),
        );
        assert_eq!(
            detect_bayer_layout(&mut buf.as_slice(), w, h),
            Some("bayer_rggb16le")
        );

        // ② 高纹理：跨通道均值差(100)远小于逐像素纹理涨落(±1500)——旧算法
        //    两对角逐像素差分比≈1 会返回 None，均值法纹理无关 → rggb
        let buf = pack(
            (0..h as usize)
                .flat_map(|r| (0..w as usize).map(move |c| (r, c)))
                .map(|(r, c)| {
                    // 基值抬高避免贴地
                    ch(r, c) / 10 + rnd() % 3000
                })
                .collect(),
        );
        assert_eq!(
            detect_bayer_layout(&mut buf.as_slice(), w, h),
            Some("bayer_rggb16le")
        );

        // ③ 干净 GBRG：G 在主对角、(0,1)=B 暗 / (1,0)=R 亮 → gbrg
        let buf = pack(
            (0..h as usize)
                .flat_map(|r| (0..w as usize).map(move |c| (r, c)))
                .map(|(r, c)| match (r % 2, c % 2) {
                    (0, 1) => 1000u64,
                    (1, 0) => 3000,
                    _ => 2000,
                })
                .collect(),
        );
        assert_eq!(
            detect_bayer_layout(&mut buf.as_slice(), w, h),
            Some("bayer_gbrg16le")
        );

        // ④ 纯灰：结构性门限拦截 → None
        let buf = pack(vec![2048; (w * h) as usize]);
        assert_eq!(detect_bayer_layout(&mut buf.as_slice(), w, h), None);

        // ⑤ 位深/对齐不变量：同排列整体 ×16（如 12bit 高对齐存 16bit 容器）
        //    → 比值统计不变，判别结果不变
        let buf = pack(
            (0..h as usize)
                .flat_map(|r| (0..w as usize).map(move |c| (r, c)))
                .map(|(r, c)| ch(r, c) * 16)
                .collect(),
        );
        assert_eq!(
            detect_bayer_layout(&mut buf.as_slice(), w, h),
            Some("bayer_rggb16le")
        );
    }

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
        svc.request(key, &src, "movie.mp4", 12345, &cfg, 0);
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
        svc.request(key2, &src, "sensor.bin", 12345, &bad, 0);
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

    /// 探针读数 = 文件存储原值（未位移未转换）+ 按格式取数位置正确：
    /// nv12 取 Y(x,y) + (x/2,y/2) 共享 U/V；bayer 取对齐 2×2 块 4 原值
    /// （bit_shift=4 时读出的是低 12bit 原值而非位移后的显示值）；帧寻址
    /// = k×frame_bytes
    #[test]
    fn probe_raw_pixel_按格式取存储原值() {
        let dir = std::env::temp_dir().join(format!(
            "rawdec_probe_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // ---- nv12：2 帧 4×4，第 1 帧 Y(3,2)=77、色度块 (1,1) U/V=(66,88) ----
        let (w, h) = (4u32, 4u32);
        let mk_nv12 = |yv: u8, u: u8, v: u8| {
            let (yp, cw) = ((w * h) as usize, (w / 2) as usize);
            let mut d = vec![0u8; yp + yp / 2];
            d[2 * w as usize + 3] = yv; // Y(3,2)
            let cc = 1 * cw + 1; // 色度坐标 (1,1)（覆盖 (3,2) 的 2×2 块）
            d[yp + cc] = u;
            d[yp + yp / 4 + cc] = v;
            d
        };
        let mut data = mk_nv12(11, 22, 33);
        data.extend(mk_nv12(77, 66, 88));
        let p = dir.join("t.nv12");
        std::fs::write(&p, &data).unwrap();
        let info = RawFrameInfo {
            w,
            h,
            pixfmt: "nv12".into(),
            fps: 10.0,
            bit_shift: 0,
            auto_bayer: false,
            frame_bytes: (w * h * 3 / 2) as u64,
            frame_count: 2,
        };
        let src = ImageSource::File(p.clone());
        let uv = egui::vec2(3.0 / 4.0, 2.0 / 4.0);
        match probe_raw_pixel(&src, &info, 0, uv).unwrap() {
            RawProbe::Nv12 { y, u, v } => assert_eq!((y, u, v), (11, 22, 33), "第 0 帧"),
            o => panic!("应 Nv12，实际 {o:?}"),
        }
        match probe_raw_pixel(&src, &info, 1, uv).unwrap() {
            RawProbe::Nv12 { y, u, v } => {
                assert_eq!((y, u, v), (77, 66, 88), "第 1 帧帧寻址正确")
            }
            o => panic!("应 Nv12，实际 {o:?}"),
        }

        // ---- bayer_rggb16le + bit_shift=4：读存储原值（低 12bit）非显示值 ----
        // 4×2 图（frame_bytes=16B=8 样本），RGGB：行 0 = R G R G、行 1 = G B G B
        let raw16 = |v: u16| v.to_le_bytes();
        let mut d = Vec::new();
        for v in [100u16, 200, 101, 201, 300, 400, 301, 401] {
            d.extend(raw16(v));
        }
        let p = dir.join("t.raw");
        std::fs::write(&p, &d).unwrap();
        let info = RawFrameInfo {
            w: 4,
            h: 2,
            pixfmt: "bayer_rggb16le".into(),
            fps: 10.0,
            bit_shift: 4,
            auto_bayer: false,
            frame_bytes: 16,
            frame_count: 1,
        };
        let src = ImageSource::File(p);
        // 采样 (3,1) → 对齐 2×2 块起点 (2,0)：行 0 [101,201]、行 1 [301,401]
        let uv = egui::vec2(3.0 / 4.0, 1.0 / 2.0);
        match probe_raw_pixel(&src, &info, 0, uv).unwrap() {
            RawProbe::Bayer {
                layout,
                block,
                bits,
            } => {
                assert_eq!(layout, "RGGB");
                assert_eq!(bits, 12, "bit_shift=4 → 有效位深 12");
                assert_eq!(block, [[101, 201], [301, 401]], "对齐块 4 个存储原值");
            }
            o => panic!("应 Bayer，实际 {o:?}"),
        }

        // 采样 (0,0)：块 = 前 4 个样本（未位移——bit_shift=4 不影响读数）
        let uv = egui::vec2(0.0, 0.0);
        match probe_raw_pixel(&src, &info, 0, uv).unwrap() {
            RawProbe::Bayer { block, .. } => assert_eq!(block, [[100, 200], [300, 400]]),
            o => panic!("应 Bayer，实际 {o:?}"),
        }

        // ---- gray16le：单点原值 ----
        let mut d = Vec::new();
        d.extend(raw16(1234));
        d.extend(raw16(5678));
        let p = dir.join("t.g16");
        std::fs::write(&p, &d).unwrap();
        let info = RawFrameInfo {
            w: 2,
            h: 1,
            pixfmt: "gray16le".into(),
            fps: 10.0,
            bit_shift: 0,
            auto_bayer: false,
            frame_bytes: 4,
            frame_count: 1,
        };
        let src = ImageSource::File(p);
        match probe_raw_pixel(&src, &info, 0, egui::vec2(0.5, 0.0)).unwrap() {
            RawProbe::Gray { v, bits } => {
                assert_eq!((v, bits), (5678, 16));
            }
            o => panic!("应 Gray，实际 {o:?}"),
        }

        // ---- yuv420p：独立 UV 平面（V 平面紧跟 U 平面之后，区别于 nv12 交错）----
        // 4×2（frame_bytes=12B：Y 8B + U 2B + V 2B），Y(3,0)=77、U[1]=66、V[1]=88
        let mut d = vec![0u8; 12];
        d[3] = 77; // Y(3,0)
        d[8 + 1] = 66; // U 平面 (1,0)
        d[8 + 2 + 1] = 88; // V 平面 (1,0)
        let p = dir.join("t.yuv420");
        std::fs::write(&p, &d).unwrap();
        let info = RawFrameInfo {
            w: 4,
            h: 2,
            pixfmt: "yuv420p".into(),
            fps: 10.0,
            bit_shift: 0,
            auto_bayer: false,
            frame_bytes: 12,
            frame_count: 1,
        };
        let src = ImageSource::File(p);
        // 采样 (3,0) → 色度 (1,0)：Y=77、U=66、V=88
        match probe_raw_pixel(&src, &info, 0, egui::vec2(3.0 / 4.0, 0.0)).unwrap() {
            RawProbe::Nv12 { y, u, v } => assert_eq!((y, u, v), (77, 66, 88)),
            o => panic!("应 Nv12（yuv420p 共用取样语义），实际 {o:?}"),
        }

        // ---- rgb24：单点 RGB 三分量（逐像素 3 字节，R,G,B 字节序）----
        // 2×1（frame_bytes=6B），像素 (1,0) = (10, 20, 30)
        let d = vec![0u8, 0, 0, 10, 20, 30];
        let p = dir.join("t.rgb");
        std::fs::write(&p, &d).unwrap();
        let info = RawFrameInfo {
            w: 2,
            h: 1,
            pixfmt: "rgb24".into(),
            fps: 10.0,
            bit_shift: 0,
            auto_bayer: false,
            frame_bytes: 6,
            frame_count: 1,
        };
        let src = ImageSource::File(p);
        match probe_raw_pixel(&src, &info, 0, egui::vec2(0.75, 0.0)).unwrap() {
            RawProbe::Rgb { r, g, b } => assert_eq!((r, g, b), (10, 20, 30), "R,G,B 字节序"),
            o => panic!("应 Rgb，实际 {o:?}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// yuv420p 软解：独立 UV 平面取样正确（4×2，Y/U/V 平面偏移与色度
    /// 半采样索引）；中性色无色偏（Y=128, U=V=128 → 灰）
    #[test]
    fn yuv420p_to_rgb_独立uv平面() {
        // Y 平面 8B：仅 (2,0)=255，其余 0；U=[128, 200]，V=[128, 128]
        let (w, h) = (4usize, 2usize);
        let mut d = vec![0u8; 12];
        d[2] = 255;
        d[8] = 128;
        d[9] = 200;
        d[10] = 128;
        d[11] = 128;
        let rgb = yuv420p_to_rgb(w, h, &d).unwrap();
        // 3 字节/像素：行 0 = [黑(0,0), 黑, 白偏蓝, 白偏蓝]、行 1 全黑
        // (2,0)：Y=255, u=200-128=72, v=0 → r=255, g=255-0.344136*72≈230, b=255
        assert_eq!(&rgb[0..3], &[0, 0, 0]);
        assert_eq!(&rgb[6..9], &[255, 230, 255], "U 平面独立取样");
        assert_eq!(&rgb[12..15], &[0, 0, 0], "行 1 复用同一色度行");
        // 中性灰：全 Y=128, U=V=128 → (128,128,128)
        let d = vec![128u8; 12];
        let rgb = yuv420p_to_rgb(w, h, &d).unwrap();
        assert!(rgb.chunks_exact(3).all(|c| c == [128, 128, 128]));
        // 数据不足报错
        assert!(yuv420p_to_rgb(w, h, &d[..11]).is_err());
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
        // yuv420p 在软解支持名单内（独立 UV 平面，见 yuv420p_to_rgb）
        let rl2 = RawLoad {
            pixfmt: "yuv420p".into(),
            ..rl
        };
        let info2 = raw_frame_info(&rl2, size).unwrap();
        assert_eq!(info2.frame_bytes, (1920 * 1080 * 12 / 8) as u64);
        // 真正不在名单的格式 → RawErr
        let rl3 = RawLoad {
            pixfmt: "yuv444p".into(),
            ..rl
        };
        assert!(raw_frame_info(&rl3, size).is_err());
    }
}
