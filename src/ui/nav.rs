//! 快捷键与导航：全屏切换、mpv 风格按键处理、同类型媒体上一个/下一个、幻灯片自动翻页。

use super::*;

impl App {
    // ---- 快捷键（参考 mpv 习惯） ----

    /// 全屏切换（回车；原双击画面已取消：与图片双击缩放习惯统一），
    /// 全屏时隐藏所有面板只留视频+进度条
    pub(super) fn set_fullscreen(&mut self, on: bool, frame: &mut eframe::Frame) {
        self.fullscreen = on;
        if let Some(w) = frame.winit_window() {
            w.set_fullscreen(if on {
                Some(winit::window::Fullscreen::Borderless(None))
            } else {
                None
            });
        }
        tracing::info!("全屏: {on}");
    }

    /// 空格=暂停/播放，←/→=±5s（图片模式=上一张/下一张；视频暂停时=上一帧/下一帧），
    /// ↑/↓=模式分治：图片=缩放步进、视频=音量±5；M=静音（仅视频；
    /// 真相在 cfg，未播放/未开文件也可预设），
    /// PgUp/PgDn=上一个/下一个，
    /// L=视频播放模式切换（列表播放↔单视频循环），
    /// N=打开下一个压缩包，B=回到锚点，回车=全屏切换，Esc=退出全屏；
    /// 图片模式：+/−/↑/↓=缩放（锚视口中心），0=100%，Home=适应窗口
    pub(super) fn handle_shortcuts(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 视频需 mpv；图片纹理模式没有播放器也放行（翻页/缩放/全屏键可用）
        if self.player.is_none() && self.tex_image_key().is_none() {
            // 未开任何文件时 M 仍可预设静音——真相在 cfg（与控制栏 ♪ 按钮一致），
            // 播放器创建时统一应用；其余快捷键无作用对象，直接返回
            if ctx.memory(|m| m.focused()) == Some(search_box_id()) {
                return;
            }
            let mut do_mute = false;
            ctx.input_mut(|i| {
                i.events.retain(|e| match e {
                    // IME 下字母键常只产 Text 事件（无 Key 事件），两种都要匹配
                    egui::Event::Text(t) if t == "m" || t == "M" => {
                        do_mute = true;
                        false
                    }
                    egui::Event::Key {
                        key: egui::Key::M,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } if modifiers.is_none() => {
                        do_mute = true;
                        false
                    }
                    _ => true,
                });
            });
            if do_mute {
                // 播放器不在场只写 cfg（同 ♪ 按钮的未播放分支）
                self.cfg.player.muted = !self.cfg.player.muted;
            }
            return;
        }
        // 只有搜索框聚焦时才让出键盘（输入搜索词需要空格/字母/方向键）。
        // 不能用"任意控件聚焦"判断：点过一次按钮/列表项后焦点常驻，
        // 会导致之后按空格永远失效（用户实测遇到的"无法恢复播放"之一）
        if ctx.memory(|m| m.focused()) == Some(search_box_id()) {
            return;
        }
        let mut seek_rel = 0.0f64;
        let mut vol_delta = 0i64;
        let (mut do_pause, mut do_mute, mut step_entry) = (false, false, 0i32);
        // 视频暂停时的方向键 = 逐帧步进（上一帧/下一帧）
        let mut frame_dir = 0i32;
        // L=视频播放模式切换（列表播放↔单视频循环）
        let mut loop_toggle = false;
        // 回车=切换全屏；Esc=退出全屏（仅全屏时消费，避免抢其它控件的 Esc）
        let mut fs_toggle = false;
        let mut fs_exit = false;
        // B=回到锚点（最后一次实际解压位置，见 App.last_focus）
        let mut go_focus = false;
        // N=打开下一个压缩包（「目录内连续看包」，仅压缩包文件模式有效）
        let mut ask_next_key = false;
        // 图片模式缩放：+/−=步进，0=100%，Home=适应窗口
        let img_mode = self.tex_image_key().is_some();
        // 音量/静音仅视频模式生效（真视频；mpv 回退图虽走播放器但无声可调）
        let vid_mode = !self.playing_is_image();
        // 视频暂停态（进 mpv 前读本地原子缓存，不阻塞）
        let vid_paused = !img_mode && self.player.as_ref().is_some_and(|p| p.paused());
        let (mut zoom_in, mut zoom_out, mut zoom_100, mut zoom_fit) = (false, false, false, false);
        // 从事件流里直接消费按键（过滤按住重复触发）：
        // 移除后同帧内其它控件（如聚焦的按钮）不会再响应，避免双重触发
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                // 中文输入法下字母键往往只产生 Text 事件（无 Key 事件）——
                // M 静音必须同时匹配两种事件，否则在 IME 开启时失灵（仅视频）
                if let egui::Event::Text(t) = e {
                    if (t == "m" || t == "M") && vid_mode {
                        do_mute = true;
                        return false;
                    }
                    // L 同理（仅视频模式；图片有自己的自动翻页开关）
                    if (t == "l" || t == "L") && !img_mode {
                        loop_toggle = true;
                        return false;
                    }
                    return true;
                }
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
                let m = *modifiers;
                match key {
                    egui::Key::Space if m.is_none() => {
                        do_pause = true;
                        false
                    }
                    egui::Key::ArrowLeft if img_mode => {
                        // 图片模式：←=上一张（视频=seek ±5s，见下方兜底臂）
                        step_entry = -1;
                        false
                    }
                    egui::Key::ArrowRight if img_mode => {
                        step_entry = 1;
                        false
                    }
                    egui::Key::ArrowLeft => {
                        // 暂停时=上一帧（逐帧），播放中=快退 5s
                        if vid_paused {
                            frame_dir = -1;
                        } else {
                            seek_rel = -5.0;
                        }
                        false
                    }
                    egui::Key::ArrowRight => {
                        if vid_paused {
                            frame_dir = 1;
                        } else {
                            seek_rel = 5.0;
                        }
                        false
                    }
                    // 缩放键（图片模式）：+/=/− 步进，0=100%，Home=适应窗口。
                    // '=' 也接放大（主键盘 '+' 需 Shift+='='，两种都收）
                    egui::Key::Plus | egui::Key::Equals if img_mode && m.is_none() => {
                        zoom_in = true;
                        false
                    }
                    egui::Key::Minus if img_mode && m.is_none() => {
                        zoom_out = true;
                        false
                    }
                    egui::Key::Num0 if img_mode && m.is_none() => {
                        zoom_100 = true;
                        false
                    }
                    egui::Key::Home if img_mode && m.is_none() => {
                        zoom_fit = true;
                        false
                    }
                    // ↑/↓ 模式分治：图片=缩放步进（与 +/− 同款），视频=音量±5；
                    // mpv 回退图两不沾（无缩放 UI、无声可调）——消费掉防焦点漂移
                    egui::Key::ArrowUp if m.is_none() => {
                        if img_mode {
                            zoom_in = true;
                        } else if vid_mode {
                            vol_delta = 5;
                        }
                        false
                    }
                    egui::Key::ArrowDown if m.is_none() => {
                        if img_mode {
                            zoom_out = true;
                        } else if vid_mode {
                            vol_delta = -5;
                        }
                        false
                    }
                    egui::Key::M if m.is_none() && vid_mode => {
                        do_mute = true;
                        false
                    }
                    // L=视频播放模式切换（图片模式不响应）
                    egui::Key::L if m.is_none() && !img_mode => {
                        loop_toggle = true;
                        false
                    }
                    egui::Key::PageUp if m.is_none() => {
                        step_entry = -1;
                        false
                    }
                    egui::Key::PageDown if m.is_none() => {
                        step_entry = 1;
                        false
                    }
                    egui::Key::B if m.is_none() => {
                        go_focus = true;
                        false
                    }
                    egui::Key::N if m.is_none() => {
                        ask_next_key = true;
                        false
                    }
                    egui::Key::Enter if m.is_none() => {
                        fs_toggle = true;
                        false
                    }
                    egui::Key::Escape if m.is_none() && self.fullscreen => {
                        fs_exit = true;
                        false
                    }
                    _ => true,
                }
            });
        });
        // 空格：视频=暂停/恢复，图片=自动翻页开关（与控制栏中间按钮一致）
        if do_pause {
            if self.playing_is_image() {
                self.slideshow_on = !self.slideshow_on;
            } else if let Some(p) = &self.player {
                p.toggle_pause();
            }
        }
        // L：视频播放模式切换（列表播放↔单视频循环）
        if loop_toggle {
            self.toggle_video_loop();
        }
        // M：静音翻转。真相在 cfg——未播放/未开文件也可预设，
        // 播放器在场才下发 mpv（不在场则等创建时统一应用）
        if do_mute {
            self.cfg.player.muted = !self.cfg.player.muted;
            if let Some(p) = &self.player {
                p.set_muted(self.cfg.player.muted);
            }
        }
        if let Some(p) = &self.player {
            if seek_rel != 0.0 {
                p.seek_relative(seek_rel);
            }
            if frame_dir != 0 {
                p.frame_step(frame_dir);
            }
            if vol_delta != 0 {
                let v = (self.cfg.player.volume + vol_delta).clamp(0, 100);
                p.set_volume(v);
                self.cfg.player.volume = v;
                // 调音量即解除静音（set_volume 已对 mpv 下发 mute=no，
                // cfg 真相同步清除，UI 图标与持久化保持一致）
                self.cfg.player.muted = false;
            }
        }
        if step_entry != 0 {
            self.play_adjacent(step_entry, step_entry > 0);
        }
        // 图片缩放键：+/− 步进（锚视口中心），0=100%，Home=适应窗口
        if zoom_in {
            self.img_zoom_step(1.25);
        }
        if zoom_out {
            self.img_zoom_step(1.0 / 1.25);
        }
        if zoom_100 {
            self.img_zoom_to(Some(1.0));
        }
        if zoom_fit {
            self.img_zoom_to(None);
        }
        // N：直接弹出下一个压缩包选择窗（不等翻到末项，主动触发无“已到末项”文案）
        if ask_next_key {
            self.ask_next_archive(false);
        }
        // B：回到锚点（与列表底部悬浮按钮一致）；正播在锚点时无事可做
        if go_focus {
            if let Some(f) = self.last_focus {
                if self.playing != Some(f) {
                    self.play(f);
                }
            }
        }
        if fs_toggle {
            self.set_fullscreen(!self.fullscreen, frame);
        }
        if fs_exit {
            self.set_fullscreen(false, frame);
        }
    }

    /// 当前播放的条目是否是图片
    pub(super) fn playing_is_image(&self) -> bool {
        self.archive
            .as_ref()
            .and_then(|a| self.playing.map(|i| &a.entries[i]))
            .map(|e| self.cfg.is_image_ext(&e.name))
            .unwrap_or(false)
    }


    /// 播放列表中的上一个/下一个媒体（offset = -1/+1）。
    /// 只在当前条目同类型内导航（视频↔视频，图片↔图片），避免视频播着切到图片。
    /// 未播放时：按当前标签页类型从第一项（下一个）/最后一项（上一个）开始。
    /// ask_next = 用户显式的"下一个"操作（快捷键/滚轮/按钮）：已在本包最后一项
    /// 再往后翻时，询问是否打开同目录下一个压缩包（config: ui.ask_next_archive）。
    pub(super) fn play_adjacent(&mut self, offset: i32, ask_next: bool) {
        let Some(a) = &self.archive else { return };
        // 未播放：类型由当前标签页决定
        let (cur, want_video) = match self.playing {
            Some(i) => (i, self.cfg.is_video_ext(&a.entries[i].name)),
            None => (usize::MAX, self.list_tab == ListTab::Video),
        };
        // 过滤视图里同类型的条目序列
        let same_kind: Vec<usize> = a
            .view
            .iter()
            .copied()
            .filter(|&i| self.cfg.is_video_ext(&a.entries[i].name) == want_video)
            .collect();
        if same_kind.is_empty() {
            return;
        }
        let ei = if cur == usize::MAX {
            // 未播放：下一个=第一项，上一个=最后一项
            if offset > 0 {
                same_kind[0]
            } else {
                same_kind[same_kind.len() - 1]
            }
        } else {
            let Some(pos) = same_kind.iter().position(|&i| i == cur) else {
                return;
            };
            let np = pos as i32 + offset;
            if np < 0 {
                return;
            }
            if (np as usize) >= same_kind.len() {
                // 已在最后一项再往后翻：弹窗选择打开同目录下一个压缩包
                // （仅用户显式操作触发；图片自动翻页传 false，防每帧重复弹窗）
                if ask_next {
                    self.ask_next_archive(true);
                }
                return;
            }
            same_kind[np as usize]
        };
        self.play(ei);
    }

    /// 图片自动播放（PPT 式翻页）：间隔到了切下一张；手动切换会重置计时
    pub(super) fn slideshow_tick(&mut self, ctx: &egui::Context) {
        if !self.slideshow_on || !self.playing_is_image() {
            return;
        }
        let now = ctx.input(|i| i.time);
        if self.slideshow_idx != self.playing {
            // 播放项变化（翻页/手动切换）：重置计时
            self.slideshow_idx = self.playing;
            self.slideshow_anchor = now;
            return;
        }
        if now - self.slideshow_anchor >= self.cfg.ui.slideshow_secs {
            // 自动翻页不触发"打开下一个压缩包"询问（到最后一张停住）
            self.play_adjacent(1, false);
        }
    }

    /// 切换视频播放模式（两态）：列表播放（默认，播完自动下一个）↔
    /// 单视频循环（mpv loop-file=inf 原生循环，EOF 不触发）。
    /// 切到循环时若已停在末帧（keep-open EOF 粘滞暂停），回开头复活继续
    pub(super) fn toggle_video_loop(&mut self) {
        self.video_loop = !self.video_loop;
        let on = self.video_loop;
        if let Some(p) = &self.player {
            p.set_loop(on);
            if on && p.eof_reached() {
                // loop-file 不回溯已发生的 EOF：手动回开头并解除粘滞暂停
                p.seek(0.0);
                p.set_paused(false);
            }
        }
        self.notice = Some(if on {
            "单视频循环".into()
        } else {
            "列表播放（播完自动下一个）".into()
        });
        tracing::info!("视频播放模式: {}", if on { "循环" } else { "列表" });
    }

    /// 列表播放：视频播到末尾自动切下一个（每帧轮询，EOF 后 keep-open
    /// 粘滞暂停使节拍回落 100ms，推进延迟 ≤100ms 不可感知）。
    /// eof_handled 防重入：推进无效果时（列表末尾停住/目标需先解压等
    /// 装载）不每帧重复触发；eof 复位（新文件开播/手动回拖）自动清空。
    /// 循环模式下 eof-reached 恒 false（mpv 原生循环不触发 EOF），天然互斥
    pub(super) fn video_eof_tick(&mut self) {
        if self.video_loop || self.playing.is_none() || self.playing_is_image() {
            return;
        }
        let eof = self.player.as_ref().is_some_and(|p| p.eof_reached());
        if !eof {
            self.eof_handled = None;
            return;
        }
        if self.eof_handled.is_none() {
            self.eof_handled = self.playing;
            // 自动推进不触发"打开下一个压缩包"询问（到最后一个停住）
            self.play_adjacent(1, false);
        }
    }
}
