//! 各面板绘制：顶栏（菜单/标签页/搜索/提示）、左侧文件列表（虚拟化）、中央媒体区（mpv 纹理 blit/解压进度覆盖层）、底部控制栏与进度条。

use super::*;

/// 进度条/播放头主题色
const ACCENT: egui::Color32 = egui::Color32::from_rgb(64, 156, 255);

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
                        ("← / →", "快退 / 快进 5 秒"),
                        ("Shift + ← / →", "快退 / 快进 1 秒"),
                        ("↑ / ↓", "音量 ±5"),
                        ("M", "静音切换"),
                        ("PgUp / PgDn", "上一个 / 下一个"),
                        ("N", "打开下一个压缩包（连续看包）"),
                        ("B", "回到锚点（最后一次解压位置）"),
                        ("鼠标滚轮（视频区）", "上一个 / 下一个"),
                        ("回车 / 双击画面", "全屏切换"),
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
                        "打开压缩包后文件列表显示在这里"
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
                        // truncate：超长文件名截断显示（全名在面板底部状态行）。
                        // 必须截断——selectable_label 默认换行会把长名行撑成 2 行高，
                        // 行高不均匀破坏虚拟化的等高前提：首行实测高被钉在 2 行值上，
                        // 渲染范围按虚高 step 缩水 → 底部空隙 + 翻页时 step 双稳态翻转
                        let resp =
                            egui::Button::selectable(playing == Some(r.idx), r.label.as_str())
                                .truncate()
                                .ui(ui);
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

    pub(super) fn video_area(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame: &mut eframe::Frame) {
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

        // 视频区；双击=全屏（单击暂停已取消：与双击冲突，改控制栏按钮）
        let resp = ui.allocate_rect(video, egui::Sense::click());
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
                    "点击左侧视频文件开始播放".to_string()
                } else {
                    "先打开一个压缩包（ZIP / 7Z / RAR）".to_string()
                };
                ui.put(
                    egui::Rect::from_center_size(video.center(), egui::vec2(400.0, 30.0)),
                    egui::Label::new(egui::RichText::new(text).weak()).selectable(false),
                );
            }
        }
        // mpv render API 纹理渲染（ARCHITECTURE.md 13，照搬开源参考实现的
        // PaintCallback 模式）：离屏 FBO ← mpv 绘制 → blit 到 egui 画布。
        // extract_wait 期间该帧不提交画面，覆盖层自然露出（取代旧"隐藏子窗口"）
        if !extract_wait && self.playing.is_some() {
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
                            let flags = mpv_ffi::render_context_update(info.rctx.as_ptr());
                            if flags & mpv_ffi::MPV_RENDER_UPDATE_FRAME != 0 {
                                let _ = mpv_ffi::render_gl_render(
                                    info.rctx.as_ptr(),
                                    info.fbo as i32,
                                    info.w,
                                    info.h,
                                );
                            }
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
                        }
                    });
                    ui.painter()
                        .add(egui::PaintCallback { rect: video, callback: Arc::new(cb) });
                }
            }
        }
        if resp.double_clicked() {
            self.set_fullscreen(!self.fullscreen, frame);
        }

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

    pub(super) fn controls(&mut self, ui: &mut egui::Ui) {
        #[derive(Default)]
        struct Act {
            step: i32,
            toggle: bool,
            slide: bool,
        }
        let mut act = Act::default();

        // 顶部：进度条（点击/拖动 seek）
        self.seek_bar(ui);

        // 下部：三按钮组（水平居中）+ 右侧音量条
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let icon_btn = |ui: &mut egui::Ui, txt: &str, tip: &str| {
                ui.add(
                    egui::Button::new(egui::RichText::new(txt).size(20.0))
                        .min_size(egui::vec2(48.0, 34.0)),
                )
                .on_hover_text(tip)
                .clicked()
            };
            // 按钮组整体居中：左侧弹性留白 = 右侧(音量+留白)宽度
            let side_w = ui.available_width() / 2.0;
            let vol_w = 150.0;
            ui.add_space((side_w - vol_w / 2.0 - 60.0).max(0.0)); // 60≈三个按钮组一半宽

            if icon_btn(ui, "<", "上一个") {
                act.step = -1;
            }
            if self.playing_is_image() {
                // 图片：中间按钮=自动翻页开关（PPT 式）
                let (txt, tip) = if self.slideshow_on {
                    ("||", "停止自动播放")
                } else {
                    ("▶", "自动播放（翻页）")
                };
                if icon_btn(ui, txt, tip) {
                    act.slide = true;
                }
            } else if let Some(p) = &self.player {
                let (txt, tip) = if p.paused() {
                    ("▶", "播放")
                } else {
                    ("||", "暂停")
                };
                if icon_btn(ui, txt, tip) {
                    act.toggle = true;
                }
            } else if icon_btn(ui, "▶", "播放（从第一项开始）") {
                // 未播放：播放按钮=开始当前标签页第一项
                act.step = 1;
            }
            if icon_btn(ui, ">", "下一个") {
                act.step = 1;
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
