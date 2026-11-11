//! 多图对比视图（方案 PLAN_compare_view.md）：文件列表多选 2~6 张图分栏
//! 同步缩放对比；raw dump（[[raw.demux]] 预设命中）走软解帧提取入栏，
//! 共享帧游标同步比对。
//!
//! 分层定位：UI 层组合功能 + ImageService 扩展，**VFS 层零改动**——各栏
//! 只是同一 VirtualFile 流的并列消费者。同步缩放语义 = 方案 A（相对各栏
//! fit 的倍率 + 归一化图像坐标 pan），不同分辨率图天然"内容对齐"。
//! 对比模式是独立第三个快捷键模态（compare_shortcuts），mpv 完全不在场。

use super::*;
use crate::rawdec::{raw_frame_info, RawFrameInfo};

pub(super) const COMPARE_MIN: usize = 2;
pub(super) const COMPARE_MAX: usize = 6;

// ---------------------------------------------------------------------------
// 类型
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(super) enum PaneKind {
    /// 静态图：ImageService 纹理路径
    Image,
    /// raw dump：软解帧提取路径
    Raw(RawFrameInfo),
    /// raw 预设参数错误 / 软解不支持该格式（占位显示错误文案，不阻塞其它栏）
    RawErr(String),
    /// 图片侧不参与对比（动画 webp 嗅探命中）：占位文案，不发起解码
    Unsupported(String),
}

#[derive(Clone)]
pub(super) struct ComparePane {
    /// entry 下标
    pub idx: usize,
    pub key: ImgKey,
    pub name: String,
    pub source: ImageSource,
    pub kind: PaneKind,
    /// 非流式源"已完整落地"缓存：流式恒 true；非流式在 build_compare_pane
    /// 时查一次 metadata，此后由 poll_extract 的 FileDone 事件置位——
    /// 替代旧版 compare_tick 每帧每栏 fs::metadata 轮询（空闲 CPU 优化）
    pub landed: bool,
}

#[derive(Default)]
pub(super) struct CompareSession {
    /// 滑动域快照：进入对比时按入口谓词过滤后的 view 索引序列（搜索/切
    /// 标签改变 view 时快照不重建，滑动在快照内进行）
    pub domain: Vec<usize>,
    /// 当前窗口各栏（长度 = 栏数；初始 = 选中项按视图序，可非连续——
    /// 首次滑动后吸附为连续窗口）
    pub panes: Vec<ComparePane>,
    /// 共享缩放：相对各栏 fit 的倍率（None = 各栏适应窗口；Some 底界
    /// 1.0 = fit，顶界 MAX_ZOOM）。**对比模式无 100% 语义**（各栏 fit
    /// 基准不同，"100%"失去单一含义）
    pub zoom: Option<f32>,
    /// 共享平移：视口中心指向的归一化图像坐标（0.5,0.5 = 居中）——
    /// 不同尺寸图"内容对齐"的关键
    pub center: egui::Vec2,
    /// 共享帧游标（raw 栏按此取帧，各栏钳制到 frame_count-1；badge 显示
    /// 错位，不要求所选文件帧数相同）
    pub cursor: usize,
    /// 主栏下标（full 高清档升级跟随最后交互的栏，其余栏保持 base 档）
    pub master: usize,
}

impl CompareSession {
    /// 全部栏的显示键（LRU 逐出保护集合）
    pub fn shown_keys(&self) -> Vec<ImgKey> {
        self.panes.iter().map(|p| p.key).collect()
    }

    /// 会话是否含多帧 raw 栏（←/→ 帧游标语义的判据，确定性非启发式）
    pub fn has_multi_frame_raw(&self) -> bool {
        self.panes
            .iter()
            .any(|p| matches!(&p.kind, PaneKind::Raw(i) if i.frame_count > 1))
    }

    /// raw 栏帧游标上限（全部栏帧数的最大值；纯静态会话 = 0）
    fn raw_max_frames(&self) -> usize {
        self.panes
            .iter()
            .map(|p| match &p.kind {
                PaneKind::Raw(i) => i.frame_count,
                _ => 1,
            })
            .max()
            .unwrap_or(1)
    }
}

// ---------------------------------------------------------------------------
// 纯几何函数（方案 A 同步语义；独立单测钉死）
// ---------------------------------------------------------------------------

/// 视口中心应指向的归一化图像坐标，按单栏约束钳制：大图（显示尺寸 >
/// 视口）不露边（center ∈ [半视口/disp, 1 − 半视口/disp]），小图归中居中。
/// 与单图模式 clamp_pan 语义同构，坐标系换成归一化
pub fn clamp_center(center: egui::Vec2, disp: egui::Vec2, viewport: egui::Vec2) -> egui::Vec2 {
    fn axis(c: f32, d: f32, vp: f32) -> f32 {
        if d <= vp || d <= 0.0 {
            0.5
        } else {
            let m = vp * 0.5 / d;
            c.clamp(m, 1.0 - m)
        }
    }
    egui::vec2(
        axis(center.x, disp.x, viewport.x),
        axis(center.y, disp.y, viewport.y),
    )
}

/// 单栏图像屏幕矩形：视口中心指向 `center`（归一化图像坐标）。
/// 推导：p + center*disp = vp.center → 中心 = vp.center + (0.5 − center)*disp
pub fn pane_image_rect(vp: egui::Rect, disp: egui::Vec2, center: egui::Vec2) -> egui::Rect {
    egui::Rect::from_center_size(
        vp.center() + (egui::vec2(0.5, 0.5) - center) * disp,
        disp,
    )
}

/// 共享锚点缩放（方案 A 的 zoom_pan 同构）：锚点（delta = 锚点相对视口
/// 中心的像素偏移）下的图像点保持不动。推导：锚点图像坐标 u = center +
/// delta/disp，缩放后锚点仍显示 u → center' = center + delta*(1/disp_before
/// − 1/disp_after)。disp 按交互栏（主栏）取值，center 是共享归一化量，
/// 各栏自动对齐
pub fn sync_zoom_center(
    center: egui::Vec2,
    delta: egui::Vec2,
    disp_before: egui::Vec2,
    disp_after: egui::Vec2,
) -> egui::Vec2 {
    fn ax(c: f32, d: f32, db: f32, da: f32) -> f32 {
        if db <= 0.0 || da <= 0.0 {
            return c;
        }
        c + d * (1.0 / db - 1.0 / da)
    }
    egui::vec2(
        ax(center.x, delta.x, disp_before.x, disp_after.x),
        ax(center.y, delta.y, disp_before.y, disp_after.y),
    )
}

/// 共享缩放步进：底界 = 1.0（fit；缩到 fit 及以下落 None），顶界 MAX_ZOOM。
/// fit 态（None）只放大不缩小（已是底线）
pub fn sync_zoom_step(zoom: Option<f32>, factor: f32) -> Option<f32> {
    match zoom {
        None => (factor > 1.0).then(|| factor.min(MAX_ZOOM)),
        Some(z) => {
            let nz = z * factor;
            if nz <= 1.0 {
                None
            } else {
                Some(nz.min(MAX_ZOOM))
            }
        }
    }
}

/// 网格布局：2=1×2，3=1×3，4=2×2，5=3+2（上 3 下 2，下行每格更宽），6=3×2。
/// 固定映射（方案定案：不加布局配置项）
pub fn grid_cells(area: egui::Rect, n: usize) -> Vec<egui::Rect> {
    let n = n.clamp(COMPARE_MIN, COMPARE_MAX);
    let row_counts: &[usize] = match n {
        2 => &[2],
        3 => &[3],
        4 => &[2, 2],
        5 => &[3, 2],
        _ => &[3, 3],
    };
    let gap = 2.0;
    let rh = ((area.height() - gap * (row_counts.len() - 1) as f32) / row_counts.len() as f32)
        .max(1.0);
    let mut out = Vec::with_capacity(n);
    let mut y = area.top();
    for rc in row_counts {
        let cw = ((area.width() - gap * (*rc as f32 - 1.0)) / *rc as f32).max(1.0);
        let mut x = area.left();
        for _ in 0..*rc {
            out.push(egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cw, rh)));
            x += cw + gap;
        }
        y += rh + gap;
    }
    out
}

// ---------------------------------------------------------------------------
// 滑动窗口纯逻辑（domain 快照上的窗口运算）
// ---------------------------------------------------------------------------

/// 整组滑动 ±1（窗口首/末栏在 domain 中的位置推算，散点初始选中项在首次
/// 滑动后吸附为连续窗口）。越界返回 None（调用方提示"已到列表开头/末尾"）
pub fn slide_window(domain: &[usize], panes: &[usize], dir: i32) -> Option<Vec<usize>> {
    let first = *panes.first()?;
    let q = domain.iter().position(|&i| i == first)?;
    let n = panes.len();
    let start = if dir > 0 { q + 1 } else { q.checked_sub(1)? };
    if start + n > domain.len() {
        return None;
    }
    Some(domain[start..start + n].to_vec())
}

/// 栏数放大：从窗口末栏之后的 domain 条目追加（跳过已在窗口中的），一个
/// 都加不了返回 None（调用方提示滑动域条目不足）
pub fn grow_window(domain: &[usize], panes: &[usize], n: usize) -> Option<Vec<usize>> {
    let last = *panes.last()?;
    let p = domain.iter().position(|&i| i == last)?;
    let mut out = panes.to_vec();
    let mut added = false;
    for &i in &domain[p + 1..] {
        if out.len() >= n {
            break;
        }
        if !out.contains(&i) {
            out.push(i);
            added = true;
        }
    }
    added.then_some(out)
}

// ---------------------------------------------------------------------------
// 入口谓词与选择
// ---------------------------------------------------------------------------

/// 对比入口谓词：静态图（gif 恒为动图，排除；webp 静态/动画按名不可分，
/// 放行后在窗格构建时以 VP8X 头嗅探分流，见 [`App::build_compare_pane`]）
/// ∨ raw 预设命中。标准视频（mpv 域）不参与——对比模式 mpv 不在场
pub(super) fn compare_eligible(cfg: &Config, name: &str) -> bool {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let anim = ext == "gif";
    (cfg.is_image_ext(name) && !anim) || cfg.raw_demux(name).is_some()
}

/// 动画 webp 嗅探：与 imgview 解码分流同一判据（image crate
/// `WebPDecoder::has_animation`，VP8X ANIM 位）。打开/解析失败一律按静态
/// 处理（不可误拦静态图；失败图交 ImageService 走正常失败路径）
pub(super) fn webp_animated(source: &ImageSource) -> bool {
    fn sniff(r: impl std::io::Read + std::io::Seek) -> Option<bool> {
        // WebPDecoder::new 要求 BufRead（imgview 同款用法），裸流套 BufReader
        let mut br = std::io::BufReader::new(r);
        let dec = image::codecs::webp::WebPDecoder::new(&mut br).ok()?;
        Some(dec.has_animation())
    }
    match source {
        ImageSource::Stream(vfs, e) => vfs.open_entry(e).ok().and_then(sniff).unwrap_or(false),
        ImageSource::File(p) => {
            std::fs::File::open(p).ok().and_then(sniff).unwrap_or(false)
        }
    }
}

/// 解码源就绪判定：流式包条目可直接读；非流式包（7z/RAR）条目必须已
/// 完整解压落地（长度一致）——半截文件发起解码必然失败，且两个解码服务
/// 的失败都是确定性的（ImageService/img_fallback、RawFrameService/errs），
/// 落地检查前置是防污染的统一闸门
pub(super) fn source_ready(source: &ImageSource, size: u64) -> bool {
    match source {
        ImageSource::Stream(..) => true,
        ImageSource::File(d) => std::fs::metadata(d).is_ok_and(|m| m.len() == size),
    }
}

impl App {
    /// 进入对比视图（右键菜单「对比显示」/ 选中态 C 键）。
    /// 合格数 <2 提示不放行；>6 取视图序前 6 张 + 提示
    pub(super) fn try_enter_compare(&mut self) {
        if self.compare.is_some() {
            return;
        }
        let Some(a) = self.archive.as_ref() else {
            self.set_notice("未打开压缩包，无法对比显示");
            return;
        };
        let pos_of = |i: usize| a.view.iter().position(|&v| v == i);
        // 选中项 ∩ 谓词，按视图序（滑动域 = 整个 view ∩ 谓词的快照）
        let mut sel: Vec<usize> = self
            .selected
            .iter()
            .copied()
            .filter(|i| pos_of(*i).is_some())
            .collect();
        sel.sort_by_key(|i| pos_of(*i).unwrap_or(usize::MAX));
        let eligible: Vec<usize> = sel
            .into_iter()
            .filter(|i| compare_eligible(&self.cfg, &a.entries[*i].name))
            .collect();
        if eligible.len() < COMPARE_MIN {
            self.set_notice(
                "对比显示需要选中 2 个以上合格条目（静态图或 raw 预设命中；动图/标准视频不参与）",
            );
            return;
        }
        let truncated = eligible.len() > COMPARE_MAX;
        let domain: Vec<usize> = a
            .view
            .iter()
            .copied()
            .filter(|i| compare_eligible(&self.cfg, &a.entries[*i].name))
            .collect();
        let panes: Vec<ComparePane> = eligible
            .into_iter()
            .take(COMPARE_MAX)
            .map(|i| self.build_compare_pane(a, i))
            .collect();
        self.enter_compare_with(domain, panes);
        if truncated {
            self.set_notice(format!(
                "选中 {} 张超出上限，前 {COMPARE_MAX} 张入栏",
                self.selected.len()
            ));
        }
    }

    fn build_compare_pane(&self, a: &super::ArchiveState, idx: usize) -> ComparePane {
        let e = &a.entries[idx];
        let key = (a.id, idx);
        let source = if a.stream {
            ImageSource::Stream(a.vfs.clone(), e.clone())
        } else {
            let dir = crate::cache::cache_dir(&self.cfg);
            ImageSource::File(crate::cache::dest_path(&dir, a.id, idx, &e.name))
        };
        let kind = match self.raw_demux_opts(&e.name, Some(e.size)) {
            Err(msg) => PaneKind::RawErr(msg),
            Ok(None) => {
                // webp 静态/动画按名不可分：动画嗅探命中 → 占位文案，
                // 不发起解码（滑动窗口把动画 webp 带进窗格时同样命中）
                if e.name.to_ascii_lowercase().ends_with(".webp") && webp_animated(&source) {
                    tracing::debug!("对比窗格排除动画 webp [{}]", e.name);
                    PaneKind::Unsupported("动画 webp 不参与对比".into())
                } else {
                    PaneKind::Image
                }
            }
            Ok(Some(rl)) => match raw_frame_info(&rl, e.size) {
                Ok(info) => PaneKind::Raw(info),
                Err(msg) => PaneKind::RawErr(msg),
            },
        };
        // landed 先于 source move 计算（ImageSource 非 Copy）
        let landed = a.stream || source_ready(&source, e.size);
        ComparePane {
            idx,
            key,
            name: e.name.clone(),
            source,
            kind,
            landed,
        }
    }
    fn enter_compare_with(&mut self, domain: Vec<usize>, panes: Vec<ComparePane>) {
        // 停播 + 清单图模式状态（对比模式 mpv 不在场）
        if let Some(p) = &self.player {
            p.stop();
        }
        self.playing = None;
        self.slideshow_on = false;
        self.hold_img = None;
        self.img_source = None;
        self.dragging = false;
        self.error = None;
        self.notice = None;
        tracing::info!("进入对比视图: {} 栏", panes.len());
        self.compare = Some(CompareSession {
            domain,
            panes,
            zoom: None,
            center: egui::vec2(0.5, 0.5),
            cursor: 0,
            master: 0,
        });
        self.compare_after_change();
    }

    /// 窗口变更（进入/滑动/增减栏）后的公共收尾：发起解码 + shown 集合 +
    /// 清理不在窗口的 raw 帧 + 预取
    fn compare_after_change(&mut self) {
        let (keys, wants, stream_prefetch, domain_after, pane_n) = {
            let Some(s) = &self.compare else { return };
            let keys = s.shown_keys();
            let wants: Vec<(ImgKey, ImageSource, String)> = s
                .panes
                .iter()
                .filter(|p| matches!(p.kind, PaneKind::Image))
                .filter(|p| p.landed)
                .map(|p| (p.key, p.source.clone(), p.name.clone()))
                .collect();
            let stream_prefetch = self.archive.as_ref().is_some_and(|a| a.stream);
            let domain_after: Vec<usize> = {
                let last = s.panes.last().map(|p| p.idx);
                let pos = last.and_then(|l| s.domain.iter().position(|&i| i == l));
                pos.map(|p| s.domain[p + 1..].to_vec()).unwrap_or_default()
            };
            (keys, wants, stream_prefetch, domain_after, s.panes.len())
        };
        self.img.set_shown_multi(keys.clone());
        self.rawdec.retain(&keys);
        let max_dim = self.cfg.ui.image_max_dim;
        for (key, source, name) in wants {
            self.img.request(key, source, max_dim, &name);
        }
        // raw 帧对齐游标（进入/滑动后游标已重置/保持，按需补发）
        self.compare_request_raw_frames();
        // 预取窗口后继静态图（仅流式包——非流式由解压会话下游预取覆盖；
        // raw 栏不预取：软解毫秒级，不值 LRU 占用）
        if stream_prefetch {
            let mut n = 0;
            for &i in &domain_after {
                if n >= pane_n {
                    break;
                }
                let (eligible_img, src, name) = {
                    let a = self.archive.as_ref().unwrap();
                    let e = &a.entries[i];
                    (
                        compare_eligible(&self.cfg, &e.name)
                            && self.raw_demux_opts(&e.name, Some(e.size)).ok().flatten().is_none(),
                        ImageSource::Stream(a.vfs.clone(), e.clone()),
                        e.name.clone(),
                    )
                };
                if !eligible_img {
                    continue;
                }
                let key = (self.archive.as_ref().unwrap().id, i);
                if self.img.has(key) || self.img.is_pending(key) {
                    continue;
                }
                self.img.request(key, src, max_dim, &name);
                n += 1;
            }
        }
    }

    /// raw 栏帧请求对齐共享游标（compare_tick 每帧调用；幂等）。
    /// 源就绪判定读 `landed` 缓存（build 时/FileDone 事件置位）——半截文件
    /// 解码必失败，且 RawFrameService.errs 是确定性失败不重试，落地前请求
    /// 会永久污染
    fn compare_request_raw_frames(&mut self) {
        let wants: Vec<(ImgKey, ImageSource, RawFrameInfo, usize)> = {
            let Some(s) = &self.compare else { return };
            s.panes
                .iter()
                .filter_map(|p| match &p.kind {
                    PaneKind::Raw(info) if p.landed => {
                        let want = s.cursor.min(info.frame_count.saturating_sub(1));
                        Some((p.key, p.source.clone(), info.clone(), want))
                    }
                    _ => None,
                })
                .collect()
        };
        for (key, source, info, want) in wants {
            self.rawdec.request(key, &source, &info, want);
        }
    }

    /// 每帧 tick（ui() 轮询）：raw 结果排空、raw 帧对齐、image 栏自愈、
    /// 非流式落地检查 + 解压会话触发
    pub(super) fn compare_tick(&mut self) {
        if self.compare.is_none() {
            return;
        }
        self.rawdec.poll();
        self.compare_request_raw_frames();
        // image 栏自愈（被挤出/在途丢失一帧内补发，同 heal_current_image）；
        // 源未落地（landed=false）不请求——防 img_fallback 确定性污染
        let (heals, master_idx, master_landed): (Vec<(ImgKey, ImageSource, String)>, Option<usize>, Option<bool>) = {
            let Some(s) = &self.compare else { return };
            let heals = s
                .panes
                .iter()
                .filter(|p| matches!(p.kind, PaneKind::Image))
                .filter(|p| !self.img.has(p.key) && !self.img.is_pending(p.key))
                .filter(|p| p.landed)
                .map(|p| (p.key, p.source.clone(), p.name.clone()))
                .collect();
            // 主栏是否已落地（landed 缓存）：false = 未落地 → 可触发解压会话
            let mi = s.panes.get(s.master).map(|p| p.idx);
            let landed = s
                .panes
                .get(s.master)
                .map(|p| p.landed);
            (heals, mi, landed)
        };
        let max_dim = self.cfg.ui.image_max_dim;
        for (key, source, name) in heals {
            tracing::warn!("对比栏图片不在缓存且无在途解码，补发请求: {name}");
            self.img.request(key, source, max_dim, &name);
        }
        // 解压会话触发：主栏为非流式源、未落地、当前解压会话未覆盖主栏时
        // 重定向 start_extract（solid 包 compute_batch 滑窗通常顺带解出窗口
        // 其它栏）。防重试风暴：密码框在场（Failed(is_pwd) 已触发）或错误
        // 横幅在场（Failed 其它错误已报）时不再触发，等用户处理
        if let Some(mi) = master_idx {
            if master_landed == Some(false)
                && self.pw_prompt.is_none()
                && self.error.is_none()
            {
                let covered = self
                    .extract
                    .as_ref()
                    .is_some_and(|j| j.entry_idx == mi || j.pending.contains_key(&mi));
                if !covered {
                    self.start_extract(mi);
                }
            }
        }
    }

    /// 整组滑动 ±1（窗口重算 + 解码请求 + 游标重置；zoom/center 保持——
    /// 扫序列时用户调好的倍率通常想沿用）
    pub(super) fn compare_slide(&mut self, dir: i32) {
        let (new_idx, n) = {
            let Some(s) = &self.compare else { return };
            let idxs: Vec<usize> = s.panes.iter().map(|p| p.idx).collect();
            (
                slide_window(&s.domain, &idxs, dir),
                s.panes.len(),
            )
        };
        let Some(idxs) = new_idx else {
            self.set_notice(if dir > 0 { "已到列表末尾" } else { "已到列表开头" });
            return;
        };
        let Some(a) = self.archive.as_ref() else { return };
        let panes: Vec<ComparePane> = idxs
            .into_iter()
            .take(n)
            .map(|i| self.build_compare_pane(a, i))
            .collect();
        if let Some(s) = &mut self.compare {
            s.panes = panes;
            s.cursor = 0;
            s.master = s.master.min(s.panes.len() - 1);
        }
        self.compare_after_change();
    }

    /// 栏数动态切换（数字键 2~6 = 窗口宽度语义）：缩小保前栏、放大追加
    pub(super) fn compare_set_count(&mut self, n: usize) {
        let n = n.clamp(COMPARE_MIN, COMPARE_MAX);
        let (cur, result) = {
            let Some(s) = &self.compare else { return };
            let idxs: Vec<usize> = s.panes.iter().map(|p| p.idx).collect();
            (
                s.panes.len(),
                if n < idxs.len() {
                    Some(idxs[..n].to_vec())
                } else if n > idxs.len() {
                    grow_window(&s.domain, &idxs, n)
                } else {
                    None
                },
            )
        };
        if n == cur {
            return;
        }
        let Some(idxs) = result else {
            if n > cur {
                self.set_notice("滑动域条目不足，无法增加栏数");
            }
            return;
        };
        // 已有栏原样保留整块状态（解码纹理在 rawdec/img 里按 key 命中，
        // pane 对象克隆即可；key = (包 id, idx) 与包绑定不变）
        let old: Vec<ComparePane> = match &self.compare {
            Some(s) => s.panes.clone(),
            None => return,
        };
        let Some(a) = self.archive.as_ref() else { return };
        let panes: Vec<ComparePane> = idxs
            .iter()
            .map(|&i| match old.iter().position(|p| p.idx == i) {
                Some(pos) => old[pos].clone(),
                None => self.build_compare_pane(a, i),
            })
            .collect();
        let Some(s) = &mut self.compare else { return };
        s.panes = panes;
        s.master = s.master.min(s.panes.len() - 1);
        self.compare_after_change();
    }

    /// 退出对比：回落单图模式（播放主栏条目，已缓存秒开）
    pub(super) fn exit_compare(&mut self) {
        let Some(s) = self.compare.take() else { return };
        self.img.clear_shown_multi();
        let idx = s
            .panes
            .get(s.master)
            .or_else(|| s.panes.first())
            .map(|p| p.idx);
        tracing::info!("退出对比视图");
        if let Some(i) = idx {
            self.play(i);
        }
    }
}

// ---------------------------------------------------------------------------
// 对比模态快捷键（独立键表；分派单位是模式，不是栏内媒体类型）
// ---------------------------------------------------------------------------

impl App {
    pub(super) fn compare_shortcuts(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 搜索框聚焦整体让位（与主键表同款）
        if ctx.memory(|m| m.focused()) == Some(search_box_id()) {
            return;
        }
        let mut slide = 0i32;
        let mut step = 0i32;
        let mut count: Option<usize> = None;
        let (mut zin, mut zout, mut zfit) = (false, false, false);
        let mut exit = false;
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = e
                else {
                    return true;
                };
                if !modifiers.is_none() {
                    return true;
                }
                match key {
                    egui::Key::Escape => {
                        exit = true;
                        false
                    }
                    egui::Key::PageUp => {
                        slide = -1;
                        false
                    }
                    egui::Key::PageDown => {
                        slide = 1;
                        false
                    }
                    egui::Key::ArrowLeft => {
                        step = -1;
                        false
                    }
                    egui::Key::ArrowRight => {
                        step = 1;
                        false
                    }
                    egui::Key::Num2 => {
                        count = Some(2);
                        false
                    }
                    egui::Key::Num3 => {
                        count = Some(3);
                        false
                    }
                    egui::Key::Num4 => {
                        count = Some(4);
                        false
                    }
                    egui::Key::Num5 => {
                        count = Some(5);
                        false
                    }
                    egui::Key::Num6 => {
                        count = Some(6);
                        false
                    }
                    egui::Key::Plus | egui::Key::Equals => {
                        zin = true;
                        false
                    }
                    egui::Key::Minus => {
                        zout = true;
                        false
                    }
                    // ↑/↓ = 缩放步进（与 +/− 同义，同单图模式心智：图片态
                    // 上下箭头即缩放；fit 为底，↓ 在 fit 态无事可做）
                    egui::Key::ArrowUp => {
                        zin = true;
                        false
                    }
                    egui::Key::ArrowDown => {
                        zout = true;
                        false
                    }
                    egui::Key::Home => {
                        zfit = true;
                        false
                    }
                    _ => true,
                }
            });
        });
        // ←/→：仅多帧 raw 会话有语义（帧游标）；纯静态会话不响应——
        // 组滑动专属 PgUp/PgDn 与 fit 态滚轮，避免与"帧步进"心智混淆
        if step != 0 {
            let multi = self
                .compare
                .as_ref()
                .is_some_and(|s| s.has_multi_frame_raw());
            if multi {
                let (cur, max) = {
                    let s = self.compare.as_ref().unwrap();
                    (s.cursor, s.raw_max_frames())
                };
                let nc = (cur as i64 + step as i64).clamp(0, max.saturating_sub(1) as i64) as usize;
                if nc != cur {
                    if let Some(s) = &mut self.compare {
                        s.cursor = nc;
                    }
                }
            }
        }
        if slide != 0 {
            self.compare_slide(slide);
        }
        if let Some(n) = count {
            self.compare_set_count(n);
        }
        if zin {
            if let Some(s) = &mut self.compare {
                s.zoom = sync_zoom_step(s.zoom, 1.25);
            }
        }
        if zout {
            if let Some(s) = &mut self.compare {
                s.zoom = sync_zoom_step(s.zoom, 1.0 / 1.25);
            }
        }
        if zfit {
            if let Some(s) = &mut self.compare {
                s.zoom = None;
                s.center = egui::vec2(0.5, 0.5);
            }
        }
        if exit {
            if self.fullscreen {
                // 全屏时 Esc 先退全屏（主键表语义），再按一次退对比
                self.set_fullscreen(false, frame);
            } else {
                self.exit_compare();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;

    // ---- 动画 webp 嗅探 ----

    #[test]
    fn webp_animated_静态为否() {
        let img = image::DynamicImage::new_rgb8(8, 6);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::WebP).unwrap();
        let dir = std::env::temp_dir().join(format!("archview_cmptest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("static.webp");
        std::fs::write(&p, buf.into_inner()).unwrap();
        assert!(!webp_animated(&ImageSource::File(p.clone())));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn webp_animated_动画为是() {
        // 夹具同 imgview::decode_动画webp逐帧：testdata/anim2.webp（ffmpeg
        // libwebp 2 帧动画，生成命令见该测试注释），缺失时跳过
        let Ok(bytes) = std::fs::read("testdata/anim2.webp") else {
            eprintln!("跳过：testdata/anim2.webp 不存在（生成命令见 imgview 测试注释）");
            return;
        };
        let dir = std::env::temp_dir().join(format!("archview_cmptest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim2.webp");
        std::fs::write(&p, &bytes).unwrap();
        assert!(webp_animated(&ImageSource::File(p.clone())));
        std::fs::remove_file(&p).ok();
    }

    // ---- 方案 A 几何 ----

    #[test]
    fn clamp_center_大图不露边() {
        let vp = vec2(400.0, 300.0);
        let disp = vec2(800.0, 600.0);
        let c = clamp_center(vec2(0.9, 0.1), disp, vp);
        assert!((c.x - 0.75).abs() < 1e-4, "{c:?}");
        assert!((c.y - 0.25).abs() < 1e-4, "{c:?}");
    }

    #[test]
    fn clamp_center_小图归中() {
        let vp = vec2(400.0, 300.0);
        let disp = vec2(200.0, 150.0);
        let c = clamp_center(vec2(0.9, 0.1), disp, vp);
        assert_eq!(c, vec2(0.5, 0.5));
    }

    #[test]
    fn pane_image_rect_居中时贴合() {
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 300.0));
        let disp = vec2(200.0, 150.0);
        let r = pane_image_rect(vp, disp, vec2(0.5, 0.5));
        assert!((r.left() - 100.0).abs() < 1e-4);
        assert!((r.top() - 75.0).abs() < 1e-4);
    }

    #[test]
    fn pane_image_rect_中心指向原点() {
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 300.0));
        let disp = vec2(800.0, 600.0);
        // center=(0,0) = 图像左上角(原点)显示在视口中心：
        // p = vp.center − center*disp = (200,150)
        let r = pane_image_rect(vp, disp, vec2(0.0, 0.0));
        assert!((r.left() - 200.0).abs() < 1e-4, "left={}", r.left());
        assert!((r.top() - 150.0).abs() < 1e-4, "top={}", r.top());
    }

    #[test]
    fn sync_zoom_center_锚点图像点不动() {
        let disp = vec2(800.0, 600.0);
        let disp2 = vec2(1600.0, 1200.0);
        let center = vec2(0.5, 0.5);
        let delta = vec2(100.0, -50.0);
        let c2 = sync_zoom_center(center, delta, disp, disp2);
        // 锚点图像坐标 u = center + delta/disp 缩放前后一致
        let u1 = center + delta / disp;
        let u2 = c2 + delta / disp2;
        assert!((u1 - u2).length() < 1e-4, "{u1:?} vs {u2:?}");
    }

    #[test]
    fn sync_zoom_step_底界落fit() {
        assert_eq!(sync_zoom_step(None, 1.25), Some(1.25));
        assert_eq!(sync_zoom_step(None, 0.8), None);
        assert_eq!(sync_zoom_step(Some(1.25), 0.8), None);
        assert_eq!(sync_zoom_step(Some(1.25), 1.25), Some(1.5625));
        assert_eq!(sync_zoom_step(Some(2.0), 1.0 / 1.25), Some(1.6));
    }

    // ---- 布局 ----

    #[test]
    fn grid_cells_形状与数量() {
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 600.0));
        for (n, rows, cols_row0) in [(2usize, 1usize, 2usize), (3, 1, 3), (4, 2, 2), (5, 2, 3), (6, 2, 3)] {
            let cells = grid_cells(area, n);
            assert_eq!(cells.len(), n, "n={n}");
            // 第一行 y 一致、第二行 y 更大
            let y0 = cells[0].top();
            assert!((cells[cols_row0 - 1].top() - y0).abs() < 1e-4);
            if rows > 1 {
                assert!(cells[cols_row0].top() > y0 + 1.0);
            }
        }
    }

    // ---- 滑动窗口 ----

    fn dom() -> Vec<usize> {
        vec![10, 20, 30, 40, 50, 60, 70, 80]
    }

    #[test]
    fn slide_前进一格() {
        let w = slide_window(&dom(), &[10, 20, 30], 1).unwrap();
        assert_eq!(w, vec![20, 30, 40]);
    }

    #[test]
    fn slide_后退一格() {
        let w = slide_window(&dom(), &[40, 50, 60], -1).unwrap();
        assert_eq!(w, vec![30, 40, 50]);
    }

    #[test]
    fn slide_越界返回none() {
        assert!(slide_window(&dom(), &[60, 70, 80], 1).is_none());
        assert!(slide_window(&dom(), &[10, 20, 30], -1).is_none());
    }

    #[test]
    fn slide_散点初始吸附连续() {
        // 选中 10, 50, 70（散点）：前滑从首栏 10 的位置 +1 起连续取 3
        let w = slide_window(&dom(), &[10, 50, 70], 1).unwrap();
        assert_eq!(w, vec![20, 30, 40]);
    }

    #[test]
    fn grow_追加后续条目() {
        let w = grow_window(&dom(), &[10, 20], 4).unwrap();
        assert_eq!(w, vec![10, 20, 30, 40]);
    }

    #[test]
    fn grow_无后续可加返回none() {
        // 窗口已含 domain 末尾且无新条目可追加
        assert!(grow_window(&dom(), &[60, 70, 80], 6).is_none());
    }

    #[test]
    fn grow_domain不足返回none() {
        // 从 [70, 80] 追加到 6 栏：70 之后只有 80（已在窗口）→ 无可加
        assert!(grow_window(&dom(), &[70, 80], 6).is_none());
    }

    #[test]
    fn grow_跳过已在窗口的条目() {
        // 散点窗口 [10, 50]：50 之后追加 60（10 不重复）
        let w = grow_window(&dom(), &[10, 50], 3).unwrap();
        assert_eq!(w, vec![10, 50, 60]);
    }
}
