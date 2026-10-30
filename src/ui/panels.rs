//! 各面板绘制：顶栏（菜单/标签页/搜索/提示）、左侧文件列表（虚拟化）、中央媒体区（mpv 纹理 blit/解压进度覆盖层）、底部控制栏与进度条。

use super::*;

/// 进度条/播放头主题色
/// 主题强调色（进度条/播放头共用）
const ACCENT: egui::Color32 = egui::Color32::from_rgb(64, 156, 255);

// ---- 帧呈现节奏探针（常驻诊断，debug 级默认静默） ----
// PaintCallback 闭包每帧重建，跨帧状态放 thread_local（回调恒在 UI/GL 线程）。
// 每 5s 汇总：绘制次数与间隔 avg/max、>25ms 长帧归因（render 侧/UI 侧）、
// mpv 换帧数、render 单次最重耗时。诊断时开启（二选一）：
//   RUST_LOG 环境变量：$env:RUST_LOG="info,archview::ui::panels=debug"
//   config.toml：log_level = "info,archview::ui::panels=debug"
// 判读：
//   avg≈16.7ms 且 gaps=0  → 连续重绘成立，卡顿在呈现之外（如刷新率不整除 fps）
//   avg 正常但 gaps 多（ui侧）→ UI 偶发长帧（debug 构建/布局/OS 调度底噪，~2 个/5s 属正常）
//   gaps 多（render侧）   → render 撞 mpv core 锁（属性轮询未短路）；
//                           首帧 1~2 个 70~90ms = 首帧 GLSL 编译（进程生命周期一次，可接受）
//   avg≈100ms             → video_live 条件未生效（回落慢节拍）
//   render最重 > 30ms     → BLOCK_FOR_TARGET_TIME=0 未生效（仍在阻塞）
#[derive(Default)]
struct Cadence {
    last: Option<std::time::Instant>,
    log_mark: Option<std::time::Instant>,
    n: u32,
    sum_ms: f64,
    max_ms: f64,
    /// >25ms 长帧且该帧 render>10ms：mpv render 侧（core 锁竞争或 GL 首编译）
    gaps_render: u32,
    /// >25ms 长帧且 render 正常：egui 布局/驱动 swap/OS 调度侧
    gaps_ui: u32,
    new_frames: u32,
    render_ms_max: f64,
}
thread_local! {
    static CAD: std::cell::RefCell<Cadence> = std::cell::RefCell::new(Cadence::default());
}

impl App {
    // ---- 各面板 ----

    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // ---- 「文件」菜单：打开压缩包 / 打开媒体目录 ----
            // rfd 对话框阻塞 UI 线程，动作在菜单闭包外执行（闭包内只置标志）
            let mut open_file = false;
            let mut open_dir = false;
            ui.menu_button("文件", |ui| {
                if ui.button("打开压缩包…").clicked() {
                    open_file = true;
                    ui.close();
                }
                if ui.button("打开媒体目录…").clicked() {
                    open_dir = true;
                    ui.close();
                }
            });
            if open_file {
                self.open_dialog();
            }
            if open_dir {
                self.open_dir_dialog();
            }
            // ---- 「缓存」菜单：打开缓存目录 / 清理缓存 ----
            let mut open_cache = false;
            let mut clean_cache = false;
            ui.menu_button("缓存", |ui| {
                if ui.button("打开缓存目录").clicked() {
                    open_cache = true;
                    ui.close();
                }
                if ui.button("清理缓存").clicked() {
                    clean_cache = true;
                    ui.close();
                }
            });
            if open_cache {
                // 打开缓存目录（系统文件管理器；缓存目录本身不存在则先创建）
                let dir = cache::cache_dir(&self.cfg);
                let _ = std::fs::create_dir_all(&dir);
                // explorer 打开目录不占用等待（spawn 后立刻返回）
                if let Err(e) = std::process::Command::new("explorer").arg(&dir).spawn() {
                    self.notice = Some(format!("打开缓存目录失败: {e}"));
                }
            }
            if clean_cache {
                let freed = cache::clean_all(&cache::cache_dir(&self.cfg));
                self.notice = Some(format!("已清理缓存 {:.1} MB", freed as f64 / 1048576.0));
                tracing::info!("手动清理缓存: {:.1} MB", freed as f64 / 1048576.0);
            }
            // 快捷键帮助（弹出面板，不占常驻空间；egui 0.36 用 from_toggle_button_response 自管开关）
            let help_btn = ui.button("快捷键 ?");
            egui::Popup::from_toggle_button_response(&help_btn)
                .show(|ui| {
                    ui.set_min_width(280.0);
                    ui.strong("快捷键");
                    ui.separator();
                    for (k, v) in [
                        ("空格", "暂停 / 播放（图片=自动翻页开关）"),
                        ("← / →", "视频 seek ±5 秒；图片 = 上/下一张"),
                        ("← / →（视频暂停时）", "上一帧 / 下一帧"),
                        ("↑ / ↓", "音量 ±5"),
                        ("M", "静音切换"),
                        ("PgUp / PgDn", "上一个 / 下一个"),
                        ("L", "播放模式切换（视频：列表 ↔ 单视频循环）"),
                        ("N", "打开下一个压缩包（连续看包）"),
                        ("B", "回到锚点（最后一次解压位置）"),
                        ("鼠标滚轮", "翻页；图片缩放态=缩放（触底回适应）"),
                        ("Ctrl+滚轮", "图片缩放（任意状态，锚光标）"),
                        ("+ / -", "图片缩放步进（锚视口中心）"),
                        ("0", "图片 100% 显示"),
                        ("Home", "图片适应窗口"),
                        ("鼠标拖拽", "平移已放大的图片"),
                        ("双击图片", "适应窗口 ↔ 100%"),
                        ("回车", "全屏切换"),
                        ("Esc", "退出全屏"),
                    ] {
                        ui.horizontal(|ui| {
                            ui.strong(k);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(v);
                                },
                            );
                        });
                    }
                });
            if let Some(n) = &self.notice {
                ui.colored_label(egui::Color32::LIGHT_GREEN, n);
            }
            if let Some(e) = &self.error {
                ui.colored_label(egui::Color32::RED, e);
            }
            let buffering = self.player.as_ref().map(|p| p.buffering()).unwrap_or(false);
            if buffering {
                // 主题感知的警示色：浅色主题下 YELLOW 不可读
                ui.colored_label(ui.visuals().warn_fg_color, "Buffering…");
            }
            // 解压/打开的详细进度不放顶栏：一行放不下长文件名（曾被截断），
            // 改在媒体区居中显示（见 video_area，超过宽限期还会隐藏视频窗口）。
            // 这里只留一个短标记（无长文本不会截断），且常显——keep 回看上游
            // 缓存、等待新文件等所有场景下后台剩余量都可见；与媒体区覆盖层
            // 信息重复无妨，视频正被观看绝不能遮挡
            if let Some(j) = &self.extract {
                let left = j.pending.len();
                ui.colored_label(
                    ui.visuals().weak_text_color(),
                    format!("后台解压中…（剩余 {left} 项）"),
                );
            }
            // 锚点落地即可点（无需等整批解完）：秒开锚点，预取批继续后台跑
            if self.extract_ready {
                if let Some(f) = self.last_focus {
                    if self.playing != Some(f) {
                        let name = self
                            .archive
                            .as_ref()
                            .map(|a| a.entries[f].name.clone())
                            .unwrap_or_default();
                        let btn = ui.button(format!("解压完成：{name}｜点击观看"));
                        if btn.clicked() {
                            self.play(f);
                        }
                        btn.on_hover_text("点击观看刚解压完成的文件（快捷键 B）");
                    }
                }
            }
        });
    }

    pub(super) fn file_list(&mut self, ui: &mut egui::Ui) {
        // 压缩包名（仅文件名，无路径；完整路径悬停显示）
        if let Some(a) = &self.archive {
            ui.add(
                egui::Label::new(egui::RichText::new(&a.name).strong())
                    .truncate()
                    .selectable(false),
            )
            .on_hover_text(a.path.display().to_string());
        } else {
            ui.weak("未打开压缩包");
        }
        ui.horizontal(|ui| {
            ui.label("Search:");
            let resp = egui::TextEdit::singleline(&mut self.search)
                .id(search_box_id())
                .show(ui)
                .response;
            if resp.changed() {
                self.rebuild_view();
            }
        });
        ui.separator();

        // 标签页切换（视频 N | 图片 M）——计数来自 rebuild_view 的缓存
        ui.horizontal(|ui| {
            let tab = |ui: &mut egui::Ui, cur: ListTab, want: ListTab, title: &str| {
                let text = if cur == want {
                    egui::RichText::new(title).strong()
                } else {
                    egui::RichText::new(title).weak()
                };
                ui.selectable_label(cur == want, text).clicked()
            };
            let mut switch = None;
            if tab(
                ui,
                self.list_tab,
                ListTab::Video,
                &format!("视频 ({})", self.list_counts.0),
            ) {
                switch = Some(ListTab::Video);
            }
            if tab(
                ui,
                self.list_tab,
                ListTab::Image,
                &format!("图片 ({})", self.list_counts.1),
            ) {
                switch = Some(ListTab::Image);
            }
            // 行缓存按标签页过滤，切换时重建
            if let Some(t) = switch {
                if t != self.list_tab {
                    self.list_tab = t;
                    self.rebuild_view();
                }
            }
        });
        ui.separator();

        // ---- 虚拟化列表：只布局可见行（万级条目从每帧 O(n) 降到 O(可见行数)）----
        let mut action: Option<usize> = None;
        let mut scrolled = false;
        // 高亮跟踪：playing 变化（快捷键/箭头切换/点击/锚点落地自动播放）
        // 时滚动到该行（居中）。虚拟化下行可能不在渲染范围内，用行号×行高
        // 换算矩形再 scroll_to_rect
        let scroll_pending = self.scrolled_to != self.playing;
        let target_row = scroll_pending
            .then(|| {
                self.playing
                    .and_then(|p| self.list_rows.iter().position(|r| r.idx == p))
            })
            .flatten();
        let mut measured: Option<f32> = None;
        // 行高：上一帧的实测值（滚动换算与实际布局零漂移）；首帧用 interact_size 估计
        let row_h = self.row_h.unwrap_or_else(|| ui.spacing().interact_size.y);
        // 预收集不可变数据，避免渲染闭包内借用 self
        let rows = &self.list_rows;
        // hover 气泡要完整包内路径（含目录段），与 rows 一样预取引用进闭包
        let entries = self.archive.as_ref().map(|a| &a.entries);
        let playing = self.playing;
        let has_archive = self.archive.is_some();
        let item_spacing_y = ui.spacing().item_spacing.y;
        // auto_shrink=false：滚动区域宽度固定占满面板，滚动条钉在右边界（不随文件名长度缩进）
        let sa = egui::ScrollArea::vertical()
            .auto_shrink(egui::Vec2b::new(false, false))
            .show_viewport(ui, |ui, viewport| {
                let total = rows.len();
                let step = row_h + item_spacing_y;
                ui.set_height((step * total as f32 - item_spacing_y).max(0.0));
                if total == 0 {
                    ui.weak(if has_archive {
                        "（无匹配媒体文件）"
                    } else {
                        "打开压缩包或媒体目录后，文件列表显示在这里"
                    });
                    return;
                }
                // 手动算可视行范围，上下各多渲染 3 行余量。
                // 不用 show_rows：它按行高估算值精确 floor/ceil，对亚像素级
                // 行高偏差敏感——累积 ~20 行少算一行（底部空隙），滚动时
                // 小数偏移令 floor/ceil 轮番进位（铺满/不铺满交替闪烁）。
                // 多余的行被 clip_rect 自动裁掉，无视觉副作用
                let mut min_row =
                    (((viewport.min.y / step).floor() as isize - 3).max(0) as usize).min(total);
                let mut max_row = (viewport.max.y / step).ceil() as usize + 3;
                if max_row > total {
                    let diff = max_row - min_row;
                    max_row = total;
                    min_row = total.saturating_sub(diff);
                }
                let range = min_row..max_row;
                // 与 show_rows 相同的布局方式：按行范围圈出矩形再渲染，
                // skip_ahead_auto_ids 保证行 widget Id 跨帧稳定（虚拟化必须）
                let y_min = ui.max_rect().top() + min_row as f32 * step;
                let y_max = ui.max_rect().top() + max_row as f32 * step;
                let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.skip_ahead_auto_ids(min_row);
                    if let Some(pos) = target_row {
                        // scroll_to_rect 的矩形必须用闭包 ui 的屏幕坐标系：
                        // 此处 max_rect().top() = 内容顶 + min_row*step，
                        // 目标行屏幕 y = top + (pos - range.start) * step。
                        // egui 消费端（ScrollArea::end）按屏幕坐标换算；若用
                        // (0, 行号×步长) 的内容相对坐标，误差 = 面板顶 - 滚动偏移，
                        // 切歌时列表会跳到错误位置（见 ui::tests 回归测试）。
                        // 必须用 isize：目标行在当前渲染范围之前（手动前翻列表后
                        // 触发高亮追踪）时 pos < range.start，usize 减法在 release
                        // 下 wrap 成天文数字 → 滚动目标被钳到列表底部
                        let y = ui.max_rect().top()
                            + (pos as isize - range.start as isize) as f32 * step;
                        let target_rect = egui::Rect::from_min_size(
                            egui::pos2(ui.max_rect().left(), y),
                            egui::vec2(ui.available_width().max(1.0), row_h),
                        );
                        ui.scroll_to_rect(target_rect, Some(egui::Align::Center));
                        scrolled = true;
                    }
                    for r in &rows[range] {
                        // truncate：超长文件名截断显示（完整路径在悬停气泡里）。
                        // 必须截断——selectable_label 默认换行会把长名行撑成 2 行高，
                        // 行高不均匀破坏虚拟化的等高前提：首行实测高被钉在 2 行值上，
                        // 渲染范围按虚高 step 缩水 → 底部空隙 + 翻页时 step 双稳态翻转
                        let mut resp =
                            egui::Button::selectable(playing == Some(r.idx), r.label.as_str())
                                .truncate()
                                .ui(ui);
                        // 悬停气泡：完整包内路径。render API 后整个窗口只有 egui
                        // 一个平面，气泡盖在视频画面上不再被 mpv 原生子窗口遮挡
                        //（wid 时代因此禁用过 hover，见 ARCHITECTURE.md 11.2）。
                        // 目录段/文件名双色显示，一眼区分
                        if let Some(es) = entries {
                            let name = es[r.idx].name.as_str();
                            let (dir, file) = match name.rfind('/') {
                                Some(p) => (&name[..p + 1], &name[p + 1..]),
                                None => ("", name),
                            };
                            resp = resp.on_hover_ui(|ui| {
                                // egui 0.36：LayoutJob/TextFormat 在 egui::text 模块下。
                                // 两种颜色拼进同一个 LayoutJob → 单 Label 渲染，
                                // 长路径按气泡宽度自然换行（两个 Label 横排则不换行）。
                                // 真彩色而非主题强弱色（用户反馈强弱色对比不够明显）：
                                // 目录段琥珀（文件夹联想）、文件名主题蓝（与进度条
                                // ACCENT 同源）；深浅主题各配一档亮度保证对比度
                                let (dir_c, file_c) = if ui.visuals().dark_mode {
                                    (
                                        egui::Color32::from_rgb(224, 172, 76),
                                        ACCENT,
                                    )
                                } else {
                                    (
                                        egui::Color32::from_rgb(146, 102, 12),
                                        egui::Color32::from_rgb(24, 98, 200),
                                    )
                                };
                                let mut job = egui::text::LayoutJob::default();
                                if !dir.is_empty() {
                                    job.append(
                                        dir,
                                        0.0,
                                        egui::text::TextFormat::simple(
                                            egui::FontId::default(),
                                            dir_c,
                                        ),
                                    );
                                }
                                job.append(
                                    file,
                                    0.0,
                                    egui::text::TextFormat::simple(
                                        egui::FontId::default(),
                                        file_c,
                                    ),
                                );
                                ui.label(job);
                            });
                        }
                        if measured.is_none() {
                            measured = Some(resp.rect.height());
                        }
                        if resp.clicked() {
                            action = Some(r.idx);
                        }
                    }
                });
            });
        if scrolled {
            self.scrolled_to = self.playing;
        }
        // 回到上次位置：盖在列表底部中央的悬浮按钮（约两行高、半透明、不占满
        // 整行——按钮四周的列表项仍可点击）。目标 = last_focus（最后一次
        // 实际解压的锚点，会话退出后保留）；正播在该位置时隐藏。快捷键 B
        if let Some(f) = self.last_focus {
            if self.playing != Some(f) {
                let entry_name = self
                    .archive
                    .as_ref()
                    .map(|a| a.entries[f].name.clone())
                    .unwrap_or_default();
                let area = sa.inner_rect;
                let strip = egui::Rect::from_min_max(
                    egui::pos2(area.left(), area.bottom() - 48.0),
                    egui::pos2(area.right(), area.bottom() - 4.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(strip), |ui| {
                    // 半透明底色：叠在列表上仍能透出底下内容
                    let bg = ui.visuals().window_fill;
                    let fill =
                        egui::Color32::from_rgba_premultiplied(bg.r(), bg.g(), bg.b(), 190);
                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new("回到锚点")
                                    .fill(fill)
                                    .corner_radius(6)
                                    .min_size(egui::vec2(0.0, 40.0)),
                            )
                            .on_hover_text(format!(
                                "回到解压锚点：{entry_name}（快捷键 B）。已落地则秒开，\
                                 未落地则跳回等待、落地自动播放"
                            ))
                            .clicked()
                        {
                            self.play(f);
                        }
                    });
                });
            }
        }
        // 行高自校正：与估计值偏差超阈值时更新（DPI/主题变化后首帧即可收敛）
        if let Some(h) = measured {
            if self.row_h.is_none() || (self.row_h.unwrap() - h).abs() > 0.01 {
                self.row_h = Some(h);
            }
        }
        if let Some(ei) = action {
            self.play(ei);
        }
    }

    pub(super) fn video_area(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // 顶部一行：当前正在预览的文件名（图片/视频通用）。
        // 放视频区上方（顶栏之下）：视频区宽，长名大多能完整放下；
        // 超出时截断。全屏不显示（全屏=纯视频+进度条）；
        // Frame::show 分配高度推进光标，video 取其后剩余区域，与子窗口零重叠
        if !self.fullscreen {
            if let Some(i) = self.playing {
                if let Some(a) = &self.archive {
                    // 序号 [index/total]（当前标签页内位置，与文件列表一致）；
                    // 放文件名前面——长名截断时序号仍可见
                    let pos = self
                        .list_rows
                        .iter()
                        .position(|r| r.idx == i)
                        .map(|p| format!("[{}/{}] ", p + 1, self.list_rows.len()));
                    let text = match pos {
                        Some(p) => format!("{p}{}", a.entries[i].name),
                        None => a.entries[i].name.clone(),
                    };
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(6, 1))
                        .show(ui, |ui| {
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            )
                            .truncate()
                            .ui(ui);
                        });
                }
            }
        }
        // 视频画面占满整个中央面板剩余区域（render API 纹理 blit，无原生子窗口）
        let video = ui.available_rect_before_wrap();

        // 媒体区；click_and_drag 为图片拖拽平移准备（视频无副作用）
        let resp = ui.allocate_rect(video, egui::Sense::click_and_drag());
        // 当前条目是否纹理直显图片（视频/动图/开关关闭 = None，走 mpv 分支）
        let img_key = self.tex_image_key();
        // 解压覆盖层显示条件：
        // - 目标 == 正在播放条目 = 预取场景：不打扰播放，走顶栏短标记
        // - 目标 ≠ 正在播放 = 用户在等新文件：
        //   · 未在播放：无画面可挡，直接显示（无闪烁问题）
        //   · 正在播 A 等 B：超过宽限期（ui.extract_overlay_delay_ms）
        //     才停绘视频帧显示覆盖层——快速解压全程不打扰 A，避免闪烁
        let extract_wait = self.extract.as_ref().is_some_and(|j| {
            self.playing != Some(j.entry_idx)
                && (self.playing.is_none()
                    || j.started.elapsed()
                        >= std::time::Duration::from_millis(self.cfg.ui.extract_overlay_delay_ms))
        });
        if extract_wait || self.playing.is_none() {
            if let Some(j) = &self.extract {
                // 解压覆盖层：媒体区居中多行显示。顶栏一行放不下长文件名
                // （曾被截断、进度条看不见），这里空间充足：完整文件名换行 +
                // 阶段进度（solid 包跳过前置文件阶段无法算百分比 → 转圈 +
                // 已解压字节数，让用户看到"在动"）+ 写文件阶段字节进度条
                let rect = egui::Rect::from_center_size(
                    video.center(),
                    egui::vec2(video.width().min(760.0), 170.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("正在后台解压（固态包需顺序解压到目标位置）")
                                .color(ui.visuals().warn_fg_color),
                        );
                        ui.add_space(6.0);
                        // 完整文件名（自动换行，不截断）
                        egui::Label::new(egui::RichText::new(&j.entry_name).small())
                            .wrap()
                            .selectable(false)
                            .ui(ui);
                        ui.add_space(8.0);
                        let human = |b: u64| {
                            if b >= 1024 * 1024 * 1024 {
                                format!("{:.1} GB", b as f64 / 1024.0 / 1024.0 / 1024.0)
                            } else {
                                format!("{:.0} MB", b as f64 / 1024.0 / 1024.0)
                            }
                        };
                        if !j.stage.is_empty() {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new());
                                ui.label(format!("{}（{}）", j.stage, human(j.done)));
                            });
                        } else if j.total > 0 {
                            ui.add(
                                egui::ProgressBar::new(
                                    (j.done as f32 / j.total as f32).clamp(0.0, 1.0),
                                )
                                .text(format!("{} / {}", human(j.done), human(j.total)))
                                .desired_width(420.0),
                            );
                        } else {
                            ui.add(egui::Spinner::new());
                        }
                        if j.batch_len > 1 {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "完成后将顺带解压后续 {} 个文件，连续翻页无需再等",
                                    j.batch_len - 1
                                ))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                        }
                    });
                });
            } else {
                let text = if let Some(j) = &self.open_job {
                    let name = j
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match &self.open_progress {
                        Some((Some(f), stage)) => {
                            format!(
                                "{stage} {:.0}% — {name}",
                                (f.clamp(0.0, 1.0) * 100.0).round()
                            )
                        }
                        Some((None, stage)) => format!("{stage} — {name}…"),
                        None => format!("正在打开 {name}…"),
                    }
                } else if self.archive.is_some() {
                    "点击左侧列表中的视频/图片开始播放".to_string()
                } else {
                    // 宽度受 put 矩形（400px）约束，文案需保持单行可容纳
                    "先打开压缩包（ZIP / 7Z / RAR）或媒体目录".to_string()
                };
                ui.put(
                    egui::Rect::from_center_size(video.center(), egui::vec2(400.0, 30.0)),
                    egui::Label::new(egui::RichText::new(text).weak()).selectable(false),
                );
            }
        }
        // mpv render API 纹理渲染（ARCHITECTURE.md 13，照搬开源参考实现的
        // PaintCallback 模式）：离屏 FBO ← mpv 绘制 → blit 到 egui 画布。
        // extract_wait 期间该帧不提交画面，覆盖层自然露出（取代旧"隐藏子窗口"）；
        // 图片模式 mpv 已 stop（idle），不提交 PaintCallback（避免黑帧盖住纹理）
        if !extract_wait && self.playing.is_some() && img_key.is_none() {
            // mpv 装载窗口 60fps 轮询：帧就绪后 mpv 不触发 update 回调、靠
            // 200ms 内部超时推进（实测切换含两段 ~200ms 空洞）。主动高频
            // repaint 让 PaintCallback 无条件 render 立即解除 mpv 的 VO 等待，
            // 检出真实首帧（render_context_update 的 FRAME 位）即停，不影响稳态功耗
            if let Some(p) = &self.player {
                if !p.has_painted_since_load() {
                    ctx.request_repaint_after(std::time::Duration::from_millis(16));
                }
            }
            let ppp = ctx.pixels_per_point();
            let phys_w = ((video.width() * ppp).round() as i32).max(1);
            let phys_h = ((video.height() * ppp).round() as i32).max(1);
            // blit 目标矩形（物理像素；GL 默认帧缓冲 y 轴向上，需用表面高度翻转）
            let surf_h =
                (ctx.input(|i| i.raw.screen_rect.map_or(0.0, |r| r.height())) * ppp).round() as i32;
            let x0 = (video.min.x * ppp).round() as i32;
            let y0 = surf_h - (video.max.y * ppp).round() as i32;
            let x1 = (video.max.x * ppp).round() as i32;
            let y1 = surf_h - (video.min.y * ppp).round() as i32;
            if let (Some(gl), Some(p)) = (&self.gl, self.player.as_mut()) {
                if let Some(info) = p.render_info(gl, phys_w, phys_h) {
                    let cb = eframe::egui_glow::CallbackFn::new(move |_cb_info, painter| {
                        let gl = painter.gl();
                        use glow::HasContext as _;
                        unsafe {
                            // egui_glow 不代管回调内的 GL 状态：进入时 BLEND（premultiplied）
                            // 与 SCISSOR_TEST 处于开启态，而 mpv 渲染与 blit 都要求干净状态
                            // ——blit 若开着 BLEND，FBO 黑边（mpv 清屏为 alpha=0）会把下层/
                            // 上一帧内容透进画面，产生套娃虚影与闪烁。全程关闭，事后还原。
                            let prev_fbo = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
                            let prev_blend = gl.is_enabled(glow::BLEND);
                            let prev_scissor = gl.is_enabled(glow::SCISSOR_TEST);
                            gl.disable(glow::BLEND);
                            gl.disable(glow::SCISSOR_TEST);
                            // bind FRAMEBUFFER 同时设置 read+draw（mpv 需要两者都指向目标 FBO）
                            gl.bind_framebuffer(
                                glow::FRAMEBUFFER,
                                Some(glow::NativeFramebuffer(
                                    std::num::NonZeroU32::new(info.fbo).unwrap(),
                                )),
                            );
                            gl.viewport(0, 0, info.w, info.h);
                            // update() 用于消费/清除待渲染标志（避免 update 回调
                            // 反复唤醒），FRAME 位同时作为真实首帧检出信号
                            // （has_painted_since_load 据此停 16ms 轮询）；
                            // render 必须无条件执行：切文件的时序下 FRAME
                            // 标志可能被上一次 UI 忙帧的 update() 提前消费（读到 0），
                            // 若据此跳过 render，mpv 单帧 Image 会互相死等——直到
                            // image-display-duration 超时 EOF 的下一次 update 才补画
                            // （实测 webp 切换黑屏 5 秒）。重复 render 只是重画当前帧
                            let flags = mpv_ffi::render_context_update(info.rctx.as_ptr());
                            if flags & mpv_ffi::MPV_RENDER_UPDATE_FRAME != 0 {
                                info.first_frame
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            // [诊断] render 单次耗时：BLOCK=0 生效应 <2ms
                            let t_render = std::time::Instant::now();
                            let _ = mpv_ffi::render_gl_render(
                                info.rctx.as_ptr(),
                                info.fbo as i32,
                                info.w,
                                info.h,
                            );
                            let render_ms = t_render.elapsed().as_secs_f64() * 1000.0;
                            // FBO → egui 默认帧缓冲 blit（FBO 数据已被 mpv 翻转，
                            // 目标矩形上下对调即与屏幕坐标一致）。
                            // read 必须显式锁定到视频 FBO：mpv 内部渲染可能把 read
                            // 绑定留在别处（如屏幕），src 读到屏幕会造成"屏幕自我拷贝"
                            // 的套娃反馈（进度条/时间/暂停按钮逐级缩小的虚影 + 闪烁）
                            gl.bind_framebuffer(
                                glow::READ_FRAMEBUFFER,
                                Some(glow::NativeFramebuffer(
                                    std::num::NonZeroU32::new(info.fbo).unwrap(),
                                )),
                            );
                            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
                            gl.blit_framebuffer(
                                0,
                                0,
                                info.w,
                                info.h,
                                x0,
                                y0,
                                x1,
                                y1,
                                glow::COLOR_BUFFER_BIT,
                                glow::LINEAR,
                            );
                            // egui_glow 约定：回调必须把 FRAMEBUFFER 成对恢复（read 不允许
                            // 残留指向本 FBO，否则污染后续依赖 read 绑定的 GL 操作）
                            match std::num::NonZeroU32::new(prev_fbo as u32) {
                                Some(id) => gl.bind_framebuffer(
                                    glow::FRAMEBUFFER,
                                    Some(glow::NativeFramebuffer(id)),
                                ),
                                // 0 = 默认帧缓冲（egui 画布）
                                None => gl.bind_framebuffer(glow::FRAMEBUFFER, None),
                            }
                            if prev_blend {
                                gl.enable(glow::BLEND);
                            }
                            if prev_scissor {
                                gl.enable(glow::SCISSOR_TEST);
                            }
                            // [诊断] 帧间隔统计（含本帧 render 耗时），每 5s 汇总一次
                            CAD.with(|c| {
                                let mut c = c.borrow_mut();
                                let now = std::time::Instant::now();
                                // 首次进入回调即打点：确认闭包真的在执行
                                if c.log_mark.is_none() {
                                    tracing::debug!("[cadence] PaintCallback 已激活");
                                }
                                if let Some(l) = c.last {
                                    let d = now.duration_since(l).as_secs_f64() * 1000.0;
                                    c.n += 1;
                                    c.sum_ms += d;
                                    c.max_ms = c.max_ms.max(d);
                                    // 长帧归因：本帧 render 侧重还是 UI 侧
                                    if d > 25.0 {
                                        if render_ms > 10.0 {
                                            c.gaps_render += 1;
                                        } else {
                                            c.gaps_ui += 1;
                                        }
                                    }
                                }
                                c.last = Some(now);
                                if flags & mpv_ffi::MPV_RENDER_UPDATE_FRAME != 0 {
                                    c.new_frames += 1;
                                }
                                c.render_ms_max = c.render_ms_max.max(render_ms);
                                let mark = *c.log_mark.get_or_insert(now);
                                if now.duration_since(mark).as_secs() >= 5 {
                                    let avg = if c.n > 0 { c.sum_ms / c.n as f64 } else { 0.0 };
                                    tracing::debug!(
                                        "[cadence] paints={} avg={:.1}ms max={:.0}ms gaps>25ms={}（render侧{} ui侧{}） mpv新帧={} render最重={:.1}ms",
                                        c.n,
                                        avg,
                                        c.max_ms,
                                        c.gaps_render + c.gaps_ui,
                                        c.gaps_render,
                                        c.gaps_ui,
                                        c.new_frames,
                                        c.render_ms_max
                                    );
                                    // 保留 last/log_mark：间隔连续、已激活只打一次
                                    let last = c.last;
                                    let log_mark = Some(now);
                                    *c = Cadence {
                                        last,
                                        log_mark,
                                        ..Cadence::default()
                                    };
                                }
                            });
                        }
                    });
                    ui.painter()
                        .add(egui::PaintCallback { rect: video, callback: Arc::new(cb) });
                }
            }
        }
        if let Some(key) = img_key {
            // ---- 图片分支：纹理直显（缩放/平移/双击），全屏走回车/Esc ----
            self.image_view(ui, &resp, video, key, !extract_wait);
        } else {
            // ---- 视频分支：全屏只走回车/Esc（双击全屏已取消：与图片双击
            // 适应窗口↔100% 的看图习惯统一，避免误触全屏）----

            // 滚轮=上一个/下一个（悬停视频区时）：以真实滚轮事件触发（帧内多事件合并
            // 方向），间隔内（config: ui.wheel_switch_secs）的后续操作忽略（防触控板连跳）。
            // 不用 smooth_scroll_delta：那是平滑插值量，一次滚轮后惯性尾巴持续多帧非零，
            // 节流窗口一过就误触发第二次切换——刚翻到末项就弹出"下一个压缩包"询问
            if resp.hovered() {
                let dy = ui.input(|i| {
                    i.events.iter().fold(0.0_f32, |acc, e| match e {
                        egui::Event::MouseWheel { delta, .. } => acc + delta.y,
                        _ => acc,
                    })
                });
                if dy != 0.0 {
                    let now = ui.input(|i| i.time);
                    if now - self.wheel_last >= self.cfg.ui.wheel_switch_secs {
                        self.wheel_last = now;
                        let dir = if dy > 0.0 { -1 } else { 1 }; // 上滚=上一个
                        self.play_adjacent(dir, dir > 0);
                    }
                }
            }
        }
    }

    /// 图片纹理直显分支：Mesh + UV 绘制（zoom/pan），拖拽平移、滚轮分层
    /// （适应窗口=翻页，放大=锚光标缩放）、双击 适应窗口↔100%
    /// （Honeyview 等主流看图软件习惯；原双击全屏改为回车/Esc）。
    /// visible=false（解压覆盖层期间）只处理视图状态不绘制
    fn image_view(
        &mut self,
        ui: &mut egui::Ui,
        resp: &egui::Response,
        video: egui::Rect,
        key: ImgKey,
        visible: bool,
    ) {
        // 记录视口（缩放快捷键 img_zoom_step 需要视口几何，绘制区域随面板拖动变化）
        self.img_viewport = video;
        // ---- 滚轮先于 frame() 处理：占位期间（解码中/缓存被挤）必须仍可翻页，
        // 否则异常态下滚轮失灵，只能靠文件列表救回 ----
        // 模式感知：fit 态裸滚轮=翻页（与视频同款节流）；缩放态（或按住
        // Ctrl 任意态）=锚光标缩放（上滚放大），缩小触底自动落回 fit——
        // 继续滚即翻页，无需先双击退出。缩放需基础纹理落地当前倍率
        // （fit 态），推迟到下方交互段处理
        let mut wheel_zoom = 0.0_f32;
        let mut wheel_anchor = None;
        if resp.hovered() {
            let (dy, cursor, ctrl) = ui.input(|i| {
                let dy = i.events.iter().fold(0.0_f32, |acc, e| match e {
                    egui::Event::MouseWheel { delta, .. } => acc + delta.y,
                    _ => acc,
                });
                (dy, i.pointer.latest_pos(), i.modifiers.ctrl)
            });
            if dy != 0.0 {
                if ctrl || self.img_view.zoom.is_some() {
                    wheel_zoom = dy;
                    wheel_anchor = cursor.filter(|p| video.contains(*p));
                } else {
                    let now = ui.input(|i| i.time);
                    if now - self.wheel_last >= self.cfg.ui.wheel_switch_secs {
                        self.wheel_last = now;
                        let dir = if dy > 0.0 { -1 } else { 1 }; // 上滚=上一个
                        self.play_adjacent(dir, dir > 0);
                    }
                }
            }
        }
        // frame() 同时推进动画时钟（动图到点切帧）；Some(rest) = 距下次切帧，
        // 定时 repaint 驱动播放（静态图 rest=None，不加轮询开销）
        // 滚轮翻页发生在本函数内（上方），而 key 是调用方进入前算好的旧值：
        // 翻页帧仍拿旧 key 调 frame，shown 会停在上一张——刚翻到、还在 LRU
        // 尾部的图在下一次预取完成挤出时不受保护（自愈虽能一帧内补发，但
        // 白解码一遍且 warn 刷屏），故翻页后重取当前 key
        let key = self.tex_image_key().unwrap_or(key);
        let Some((tex, rest)) = self.img.frame(key) else {
            // 解码未就绪（非 solid 大图解压+解码可达数秒）：优先原样重画上一张
            // （HoldImg 快照）避免闪烁；无快照（首图）或等待超 2s（疑似无限
            // 解压/卡死，用户需明确感知）退回占位提示并显示已等待时长
            if visible {
                let hold = self
                    .hold_img
                    .as_ref()
                    .filter(|h| h.since.elapsed() < std::time::Duration::from_secs(2));
                match hold {
                    Some(h) => {
                        let mut mesh = egui::Mesh::with_texture(h.tex.id());
                        mesh.add_rect_with_uv(
                            h.rect,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                        ui.painter_at(video).add(egui::Shape::mesh(mesh));
                        // 新图随时可能就绪：高频轮询缩短替换延迟
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(50));
                    }
                    None => {
                        let wait = self
                            .hold_img
                            .as_ref()
                            .map(|h| h.since.elapsed())
                            .unwrap_or_default();
                        ui.put(
                            egui::Rect::from_center_size(video.center(), egui::vec2(300.0, 30.0)),
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "正在解码图片… 已等待 {:.1}s",
                                    wait.as_secs_f64()
                                ))
                                .weak(),
                            )
                            .selectable(false),
                        );
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(200));
                    }
                }
            }
            return;
        };
        if let Some(rest) = rest {
            ui.ctx().request_repaint_after(rest);
        }
        let native = tex.size_vec2();
        let fit = fit_scale(video, native);
        let zoom = self.img_view.zoom.unwrap_or(fit);
        // ---- 高清纹理升级（full 单槽，×ppp×2 clamp 8192）：深放大时从
        // 原图二次解码换高密度纹理。资格：基础纹理最长边 ≤ image_max_dim
        // ——恒等于原图 ≤ image_max_dim（阈值型预放大的图必有 base=
        // 原图×ppp ≤ 上限；超大图原尺寸直进后 base=原图 > 上限自动出
        // 局，二次解码秒级 + 180MB 级显存不值当）。触发判据与下方换画
        // 判据一致（zoom×ppp > image_full_threshold，config 可调、默认
        // 2.0）：full 是 ×2 密度纹理，采样比（zoom×ppp/2）≥1 才有肉眼
        // 增益——156%/195% 档它反被 0.78~0.98× 缩小采样，×2 Lanczos
        // 的 ~2px 边缘坡与 base GPU 直放大等宽（195% 实拍 A/B 无差），
        // 不为此白付解码+显存；滚轮/键盘 ×1.25 档位序列为
        // 100/125/156/195/244/…%，死区取 2.0 落在 195% 档（×1.25³）与
        // full 接管的 244% 档（×1.25⁴）之间，判据用 >，若恰取落地档值
        // 会因浮点表示踩空，留余量防临界抖动。
        // 就绪后下帧无缝换画。img_source 由
        // play_image/load_extracted/heal 记录，切视频即清（见 extract）----
        let native_max = native.max_elem() as u32;
        let ppp = ui.ctx().pixels_per_point();
        if native_max <= self.cfg.ui.image_max_dim
            && zoom * ppp > self.cfg.ui.image_full_threshold
            && self.img.is_static(key)
            && !self.img.has_full(key)
            && !self.img.is_full_pending(key)
            && !self.img.is_full_failed(key)
        {
            if let Some((src_key, source, name)) = self.img_source.as_ref() {
                if *src_key == key {
                    let (source, name) = (source.clone(), name.clone());
                    // 预放大倍率（ppp×2）由 ImageService 按屏幕 DPI 内部取值；
                    // 锐化开关 = image_full_sharpen 配置（Lanczos 放大后叠加
                    // unsharp mask 观感补偿）
                    self.img
                        .request_full(key, source, &name, self.cfg.ui.image_full_sharpen);
                }
            }
        }

        // ---- 交互：先改 zoom/pan，绘制时统一 clamp ----
        // 拖拽平移（fit 模式图像不大于视口，clamp 会归零，无副作用）
        if resp.dragged() {
            self.img_view.pan += resp.drag_delta();
        }
        // 滚轮缩放（上滚放大；fit 态先落地为当前倍率再乘，与快捷键缩放
        // 同款语义——zoom 已 unwrap_or(fit)；锚光标）。缩小方向触底落地：
        // 下一步倍率 ≤ fit 直接回适应窗口并闪现提示。fit_scale 不封顶
        // （小图 fit>1 放大铺满）→ 判据对任何图统一成立；max(MIN_ZOOM)
        // 兜底病态小视口（fit < MIN_ZOOM）下永不触发的卡死
        if wheel_zoom != 0.0 {
            if let Some(pos) = wheel_anchor {
                // ×1.25 与键盘 +/−（nav.rs）同档；1.25×0.8=1 一步精确抵消
                let factor = if wheel_zoom > 0.0 { 1.25 } else { 1.0 / 1.25 };
                if factor < 1.0 && (zoom * factor).max(MIN_ZOOM) <= fit {
                    self.img_view = ImgView::default();
                    self.img_fit_flash = Some(std::time::Instant::now());
                } else {
                    self.img_zoom_at(zoom, factor, pos, video);
                }
            }
        }
        // 双击：适应窗口 ↔ 100%（锚光标；非全屏——全屏走回车/Esc）
        if resp.double_clicked() {
            let cursor = ui
                .input(|i| i.pointer.latest_pos())
                .unwrap_or_else(|| video.center());
            match self.img_view.zoom {
                // fit → 100%：光标下的图像点保持不动。
                // 100% = 纹素:物理像素 1:1（zoom=1/ppp）——zoom 是相对
                // 基础纹理像素的倍率，显示物理尺寸 = 纹理×zoom×ppp；
                // 写 1.0 会在高 DPI 屏过扫 ×ppp 且误触 full 升级
                None => {
                    let z100 = 1.0 / ppp;
                    let pan = zoom_pan(self.img_view.pan, fit, z100, cursor - video.center());
                    self.img_view = ImgView { zoom: Some(z100), pan };
                }
                // 100%（或任意倍率）→ 适应窗口
                Some(_) => {
                    self.img_view = ImgView::default();
                    self.img_fit_flash = Some(std::time::Instant::now());
                }
            }
        }

        // 交互段（拖拽/滚轮/双击）刚更新过 self.img_view：此处必须重取
        // zoom。函数开头取的局部 zoom 是交互前旧值，拿"旧 zoom × 新 pan"
        // 绘制会几何错位一帧（每次缩放画面跳动一下的根因），下一帧才恢复
        // （上方触发块用旧值仅晚一帧发起解码，无碍）
        let zoom = self.img_view.zoom.unwrap_or(fit);
        // ---- 绘制：图像中心 clamp 在视口内（拖不丢，总有部分可见）----
        self.img_view.pan = clamp_pan(self.img_view.pan, native * zoom, video);
        if !visible {
            return;
        }
        // full 纹理选画：矩形/UV 按基础纹理算（同宽高比 → 屏幕矩形与
        // 全图 UV 对两种分辨率通用，切换无缩放跳变）。仅当屏幕物理像素
        // 超过基础纹理 image_full_threshold 倍（与上方触发判据一致）才换
        // full 纹理——默认 2.0 即 244% 档（×1.25⁴）起由 ×ppp×2 的 full
        // 接管，相对 full 采样比 ≥1.22× 纯放大（无 mipmap 直采无缩小带）；
        // 更浅档（100%~195%）base GPU 直放大与之肉眼无差（195% 实拍 A/B
        // 证实），不白付二次解码。屏幕密度按物理像素算：DPI 缩放下
        // 1 逻辑像素 = ppp 物理像素
        let ppp = ui.ctx().pixels_per_point();
        let screen_px = (native * zoom * ppp).max_elem();
        // 换画阈值同触发阈值：image_full_threshold 配置（默认 2.0 =
        // 越过 244% 滚轮档），两处判据必须一致否则触发/换画错位
        let want_full = screen_px > native.max_elem() * self.cfg.ui.image_full_threshold;
        let full = want_full.then(|| self.img.full_tex(key)).flatten();
        // 换画事件：同 key 下 base↔full 切换时才打印一条（切图时新 key
        // 静默重置，不逐帧刷屏），与"发起高清解码/纹理上传 full"串联成链
        let pick = (key, full.is_some());
        if self.img_pick_full.is_some_and(|(k, _)| k == key)
            && self.img_pick_full != Some(pick)
        {
            let desc = full.as_ref().map_or_else(
                || "→ base".to_string(),
                |t| format!("→ full {}x{}", t.size_vec2().x as i32, t.size_vec2().y as i32),
            );
            tracing::debug!(
                "换画[{key:?}] badge={:.0}% base={}x{} {desc}",
                zoom * ppp * 100.0,
                native.x as i32,
                native.y as i32
            );
        }
        self.img_pick_full = Some(pick);
        let tex = full.unwrap_or(tex);
        // 物理像素对齐：100%（纹素:物理像素 1:1）下矩形若落在分数物理
        // 坐标（逻辑坐标 ×ppp 常见 .5），双线性把每个输出像素变成相邻
        // 纹素的加权混合 → 整幅均匀发糊；尺寸的 f32 误差（如
        // 3392/1.5×1.5=3391.9995）还会让 LOD 微偏混入 mip1。egui 只对
        // 文字自动像素对齐，自绘 mesh 必须自己 snap：物理空间取整换回
        // 逻辑坐标——100% 即严格 1 纹素 = 1 物理像素；其余倍率仅产生
        // <0.5px 偏移（顺带消除拖拽时的亚像素爬动）。
        let raw = egui::Rect::from_center_size(video.center() + self.img_view.pan, native * zoom);
        let snapped = egui::Rect::from_min_max(
            (raw.min * ppp).round() / ppp,
            (raw.max * ppp).round() / ppp,
        );
        let rect = if snapped.width() > 0.0 && snapped.height() > 0.0 {
            snapped
        } else {
            raw
        };
        let mut mesh = egui::Mesh::with_texture(tex.id());
        mesh.add_rect_with_uv(
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        // painter_at 以媒体区裁剪：放大后超出部分不画到面板外
        ui.painter_at(video).add(egui::Shape::mesh(mesh));
        // 成功绘制即刷新防闪快照（纹理 Arc 克隆，廉价）：下次切换在解码间隙
        // 原样重画此画面。since=now → elapsed 重新从 0 计"等待下一张"时长
        self.hold_img = Some(HoldImg {
            tex: tex.clone(),
            rect,
            since: std::time::Instant::now(),
        });
        // ---- 模式指示徽标（媒体区右下角）：缩放态常驻倍率百分比（100% =
        // 纹素:物理像素 1:1 基准，故乘 ppp）；落回适应窗口闪现"适应窗口"
        // 约 1s（提示"再滚=翻页"）；fit 阅读态无徽标。闪现期内驱动重绘，
        // 到期徽标才消失
        if let Some(z) = self.img_view.zoom {
            self.paint_badge(ui, video, &format!("{}%", (z * ppp * 100.0).round()));
        } else if let Some(t) = self.img_fit_flash {
            if t.elapsed() < std::time::Duration::from_secs(1) {
                self.paint_badge(ui, video, "适应窗口");
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }

    /// 媒体区右下角小徽标（黑底半透明圆角 + 白字）：缩放模式指示用
    fn paint_badge(&self, ui: &egui::Ui, area: egui::Rect, text: &str) {
        let font = egui::TextStyle::Body.resolve(ui.style());
        let galley = ui
            .painter()
            .layout_no_wrap(text.to_string(), font, egui::Color32::WHITE);
        let pad = 6.0;
        let size = galley.size() + egui::vec2(pad * 2.0, pad);
        let margin = 8.0;
        let rect = egui::Rect::from_min_size(
            area.right_bottom() - size - egui::vec2(margin, margin),
            size,
        );
        let painter = ui.painter_at(area);
        painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(160));
        painter.galley(rect.left_top() + egui::vec2(pad, pad * 0.5), galley, egui::Color32::WHITE);
    }

    /// 锚点缩放：保持锚点下的图像点不动（锚点 rel 相对视口中心）。
    /// 滚轮/快捷键共用；倍率 clamp 在 [MIN_ZOOM, MAX_ZOOM]
    fn img_zoom_at(
        &mut self,
        cur_zoom: f32,
        factor: f32,
        anchor: egui::Pos2,
        viewport: egui::Rect,
    ) {
        let new_zoom = (cur_zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.img_view.pan =
            zoom_pan(self.img_view.pan, cur_zoom, new_zoom, anchor - viewport.center());
        self.img_view.zoom = Some(new_zoom);
    }

    /// 快捷键缩放一步（锚视口中心）：factor > 1 放大，< 1 缩小。
    /// fit 模式先落地为当前 fit 倍率再乘（视口变化时 fit 是实时值）
    pub(super) fn img_zoom_step(&mut self, factor: f32) {
        let Some(key) = self.tex_image_key() else { return };
        let viewport = self.img_viewport;
        if !viewport.is_finite() || viewport.width() <= 0.0 || viewport.height() <= 0.0 {
            return;
        }
        let Some((tex, _)) = self.img.frame(key) else { return };
        let fit = fit_scale(viewport, tex.size_vec2());
        let cur = self.img_view.zoom.unwrap_or(fit);
        self.img_zoom_at(cur, factor, viewport.center(), viewport);
    }

    /// 快捷键直接设倍率：Some(z)=逻辑倍率（1.0=100%，锚视口中心。内部
    /// 换算为纹素 1:1 的 1/ppp，与双击同语义）；None = 适应窗口
    pub(super) fn img_zoom_to(&mut self, target: Option<f32>) {
        let Some(key) = self.tex_image_key() else { return };
        let viewport = self.img_viewport;
        if !viewport.is_finite() {
            return;
        }
        match target {
            None => {
                self.img_view = ImgView::default();
                self.img_fit_flash = Some(std::time::Instant::now());
            }
            Some(z) => {
                let Some((tex, _)) = self.img.frame(key) else { return };
                let z = z / self.img.ppp();
                let fit = fit_scale(viewport, tex.size_vec2());
                let cur = self.img_view.zoom.unwrap_or(fit);
                self.img_view.pan = zoom_pan(
                    self.img_view.pan,
                    cur,
                    z,
                    viewport.center() - viewport.center(),
                );
                self.img_view.zoom = Some(z);
            }
        }
    }

    pub(super) fn controls(&mut self, ui: &mut egui::Ui) {
        #[derive(Default)]
        struct Act {
            step: i32,
            toggle: bool,
            slide: bool,
            loop_toggle: bool,
        }
        let mut act = Act::default();

        // 顶部：进度条（点击/拖动 seek）
        self.seek_bar(ui);

        // 下部：按钮组（水平居中）+ 右侧音量条
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            // selected=true 时图标着色加粗（激活态高亮，如循环播放中）
            let icon_btn = |ui: &mut egui::Ui, txt: &str, tip: &str, selected: bool| {
                let mut rt = egui::RichText::new(txt).size(20.0);
                if selected {
                    rt = rt.color(ACCENT).strong();
                }
                ui.add(egui::Button::new(rt).min_size(egui::vec2(48.0, 34.0)))
                    .on_hover_text(tip)
                    .clicked()
            };
            // 按钮组整体居中：左侧弹性留白 = 右侧(音量+留白)宽度
            let side_w = ui.available_width() / 2.0;
            let vol_w = 150.0;
            ui.add_space((side_w - vol_w / 2.0 - 84.0).max(0.0)); // 84≈四个按钮组一半宽

            if icon_btn(ui, "<", "上一个", false) {
                act.step = -1;
            }
            if self.playing_is_image() {
                // 图片：中间按钮=自动翻页开关（PPT 式）
                let (txt, tip) = if self.slideshow_on {
                    ("||", "停止自动播放")
                } else {
                    ("▶", "自动播放（翻页）")
                };
                if icon_btn(ui, txt, tip, self.slideshow_on) {
                    act.slide = true;
                }
            } else if let Some(p) = &self.player {
                let (txt, tip) = if p.paused() {
                    ("▶", "播放")
                } else {
                    ("||", "暂停")
                };
                if icon_btn(ui, txt, tip, false) {
                    act.toggle = true;
                }
            } else if icon_btn(ui, "▶", "播放（从第一项开始）", false) {
                // 未播放：播放按钮=开始当前标签页第一项
                act.step = 1;
            }
            if icon_btn(ui, ">", "下一个", false) {
                act.step = 1;
            }
            // 播放模式（仅视频，图片不显示——图片用各自的自动翻页开关）：
            // 列表播放（默认，播完自动下一个）↔ 单视频循环；循环中高亮。
            // 未播放时也显示，允许预设模式
            if !self.playing_is_image() {
                let (icon, tip) = if self.video_loop {
                    ("↻", "循环播放中：点击切为列表播放（L）")
                } else {
                    ("≡", "列表播放中：播完自动下一个｜点击切为循环（L）")
                };
                if icon_btn(ui, icon, tip, self.video_loop) {
                    act.loop_toggle = true;
                }
            }

            // 右侧：音量条（右对齐，固定宽度；始终显示，播放器未就绪时只存配置）
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut vol = self.cfg.player.volume;
                let resp = ui.add_sized(
                    [vol_w, ui.available_height().max(20.0)],
                    egui::Slider::new(&mut vol, 0..=100).text("音量"),
                );
                if resp.changed() {
                    self.cfg.player.volume = vol;
                    if let Some(p) = &self.player {
                        p.set_volume(vol); // 内部会顺带解除静音
                    }
                }
                // 静音时在音量条左侧显示红色提示（点击解除静音）
                let muted = self.player.as_ref().map(|p| p.muted()).unwrap_or(false);
                if muted {
                    let m = ui
                        .selectable_label(
                            false,
                            egui::RichText::new("静音")
                                .color(egui::Color32::RED)
                                .strong(),
                        )
                        .on_hover_text("已静音（M 键切换）；点击此处或调整音量可解除");
                    if m.clicked() {
                        if let Some(p) = &self.player {
                            p.toggle_mute();
                        }
                    }
                }
            });
        });

        // 闭包外应用动作（避免嵌套借用 self）
        if act.step != 0 {
            self.play_adjacent(act.step, act.step > 0);
        }
        if act.toggle {
            if let Some(p) = &self.player {
                p.toggle_pause();
            }
        }
        if act.slide {
            self.slideshow_on = !self.slideshow_on;
        }
        if act.loop_toggle {
            self.toggle_video_loop();
        }
    }

    /// 控制栏顶部的进度条（无播放器时只画空轨道）。
    /// 点击直接跳转；拖动实时预览、松手才 seek（避免拖动中频繁 seek 卡顿）
    pub(super) fn seek_bar(&mut self, ui: &mut egui::Ui) {
        let (dur, pos) = match &self.player {
            Some(p) => (p.duration().unwrap_or(0.0), p.time_pos().unwrap_or(0.0)),
            None => (0.0, 0.0),
        };
        let cur = if self.dragging { self.drag_pos } else { pos };

        let bar_rect =
            egui::Rect::from_min_size(ui.max_rect().min, egui::vec2(ui.available_width(), 18.0));
        let resp = ui
            .allocate_rect(bar_rect, egui::Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let painter = ui.painter_at(bar_rect);
        let y = bar_rect.center().y;
        // 两侧时间标签各预留固定宽度，轨道占中间全部剩余宽度
        let label_w = 64.0;
        let track = egui::Rect::from_min_max(
            egui::pos2(bar_rect.left() + label_w + 8.0, y - 3.0),
            egui::pos2(bar_rect.right() - label_w - 8.0, y + 3.0),
        );
        let font = egui::FontId::proportional(12.5);
        painter.text(
            egui::pos2(bar_rect.left(), y),
            egui::Align2::LEFT_CENTER,
            fmt_time(cur),
            font.clone(),
            egui::Color32::GRAY,
        );
        painter.text(
            egui::pos2(bar_rect.right(), y),
            egui::Align2::RIGHT_CENTER,
            fmt_time(dur),
            font,
            egui::Color32::GRAY,
        );
        // 轨道背景 + 已播进度 + 播放头旋钮
        painter.rect_filled(track, 2.0, egui::Color32::from_gray(70));
        if dur > 0.0 {
            let frac = (cur / dur).clamp(0.0, 1.0) as f32;
            let knob_x = track.left() + track.width() * frac;
            let filled = egui::Rect::from_min_max(track.min, egui::pos2(knob_x, track.bottom()));
            painter.rect_filled(filled, 2.0, ACCENT);
            painter.circle(egui::pos2(knob_x, y), 5.0, ACCENT, egui::Stroke::NONE);
        }

        // 交互
        if dur > 0.0 {
            let ptr_val =
                |x: f32| ((x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0) as f64 * dur;
            if resp.drag_started() || resp.clicked() {
                self.dragging = true;
                if let Some(pt) = resp.interact_pointer_pos() {
                    self.drag_pos = ptr_val(pt.x);
                }
            }
            if resp.dragged() {
                if let Some(pt) = resp.interact_pointer_pos() {
                    self.drag_pos = ptr_val(pt.x);
                }
            }
            if resp.drag_stopped() || resp.clicked() {
                self.dragging = false;
                if let Some(p) = &self.player {
                    p.seek(self.drag_pos);
                }
            }
        }
    }
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0) as u64;
    let (h, m, s) = (t / 3600, t % 3600 / 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
