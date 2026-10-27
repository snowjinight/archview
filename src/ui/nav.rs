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

    /// 空格=暂停/播放，←/→=±5s（图片模式=上一张/下一张），Shift+←/→=±1s，
    /// ↑/↓=音量±5，M=静音，PgUp/PgDn=上一个/下一个，
    /// N=打开下一个压缩包，B=回到锚点，回车=全屏切换，Esc=退出全屏；
    /// 图片模式：+/−=缩放（锚视口中心），0=100%，Home=适应窗口
    pub(super) fn handle_shortcuts(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 视频需 mpv；图片纹理模式没有播放器也放行（翻页/缩放/全屏键可用）
        if self.player.is_none() && self.tex_image_key().is_none() {
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
        // 回车=切换全屏；Esc=退出全屏（仅全屏时消费，避免抢其它控件的 Esc）
        let mut fs_toggle = false;
        let mut fs_exit = false;
        // B=回到锚点（最后一次实际解压位置，见 App.last_focus）
        let mut go_focus = false;
        // N=打开下一个压缩包（「目录内连续看包」，仅压缩包文件模式有效）
        let mut ask_next_key = false;
        // 图片模式缩放：+/−=步进，0=100%，Home=适应窗口
        let img_mode = self.tex_image_key().is_some();
        let (mut zoom_in, mut zoom_out, mut zoom_100, mut zoom_fit) = (false, false, false, false);
        // 从事件流里直接消费按键（过滤按住重复触发）：
        // 移除后同帧内其它控件（如聚焦的按钮）不会再响应，避免双重触发
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                // 中文输入法下字母键往往只产生 Text 事件（无 Key 事件）——
                // M 静音必须同时匹配两种事件，否则在 IME 开启时失灵
                if let egui::Event::Text(t) = e {
                    if t == "m" || t == "M" {
                        do_mute = true;
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
                        seek_rel = if m.shift { -1.0 } else { -5.0 };
                        false
                    }
                    egui::Key::ArrowRight => {
                        seek_rel = if m.shift { 1.0 } else { 5.0 };
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
                    egui::Key::ArrowUp if m.is_none() => {
                        vol_delta = 5;
                        false
                    }
                    egui::Key::ArrowDown if m.is_none() => {
                        vol_delta = -5;
                        false
                    }
                    egui::Key::M if m.is_none() => {
                        do_mute = true;
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
        if let Some(p) = &self.player {
            if seek_rel != 0.0 {
                p.seek_relative(seek_rel);
            }
            if do_mute {
                p.toggle_mute();
            }
            if vol_delta != 0 {
                let v = (self.cfg.player.volume + vol_delta).clamp(0, 100);
                p.set_volume(v);
                self.cfg.player.volume = v;
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
        // N：直接弹出下一个压缩包选择窗（不等翻到末项）
        if ask_next_key {
            self.ask_next_archive();
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
                    self.ask_next_archive();
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
}
