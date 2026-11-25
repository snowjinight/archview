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

    /// 列表选择快捷键：Ctrl+A 全选当前列表 / Ctrl+Q 清除全部选中
    /// （2026-11-24 新增，与 Ctrl+A 成对；§15.5 多选基建）。让位：搜索框
    /// 聚焦（文本框原生收键）、密码/编码确认窗在场（它们的 TextEdit 要
    /// 收事件）。独立成方法：先于对比模态分派调用（两模态都可用），
    /// 且 handle_shortcuts 携 eframe::Frame 构不出测试态，抽出便于单测
    pub(super) fn selection_shortcuts(&mut self, ctx: &egui::Context) {
        if self.pw_prompt.is_none()
            && self.encode_dialog.is_none()
            && !self.list_rows.is_empty()
            && ctx.memory(|m| m.focused()) != Some(search_box_id())
        {
            let mut select_all = false;
            let mut clear_sel = false;
            ctx.input_mut(|i| {
                i.events.retain(|e| {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } = e
                    {
                        // command = Ctrl（或 mac Cmd），与 Ctrl+A 既有语义一致
                        if modifiers.command {
                            match key {
                                egui::Key::A => {
                                    select_all = true;
                                    return false;
                                }
                                egui::Key::Q => {
                                    clear_sel = true;
                                    return false;
                                }
                                _ => {}
                            }
                        }
                    }
                    true
                });
            });
            if select_all {
                self.selected = self.list_rows.iter().map(|r| r.idx).collect();
            }
            if clear_sel {
                let n = self.selected.len();
                self.selected.clear();
                if n > 0 {
                    tracing::info!("Ctrl+Q 清除全部选中（{n} 项）");
                }
            }
        }
    }

    /// Ctrl+S=手动同步评级到文件（=右键菜单同项）。评级脏表是全局态
    /// （单图/对比模式评星都写入），同步入口必须两模态可达——调用点在
    /// handle_shortcuts 中先于对比模态分派。防重入在 start_rating_sync
    /// 内部（job 在跑直接忽略）。独立成方法便于单测（同 selection_shortcuts）
    pub(super) fn rating_sync_shortcut(&mut self, ctx: &egui::Context) {
        if self.config_editor.is_some() {
            return;
        }
        let sync_now = ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::S,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } if modifiers.ctrl
                )
            })
        });
        if sync_now {
            self.start_rating_sync(false);
        }
    }

    /// 空格=暂停/播放，←/→=±5s（图片模式=上一张/下一张；视频暂停时=上一帧/下一帧），
    /// ↑/↓=模式分治：图片=缩放步进、视频=音量±5；M=静音（仅视频；
    /// 真相在 cfg，未播放/未开文件也可预设），
    /// PgUp/PgDn=上一个/下一个，
    /// L=视频播放模式三态循环（定格→列表→循环），
    /// N=打开下一个压缩包，B=回到锚点，回车=全屏切换，Esc=退出全屏；
    /// 图片模式：+/−/↑/↓=缩放（锚视口中心），0=100%，Home=适应窗口，
    /// R=顺时针旋转 90°，1-5=评星，Q=清除评级；Ctrl+A=全选、Ctrl+Q=清除
    /// 全部选中、Ctrl+S=同步评级到文件；Ctrl+Delete=删除选中文件（目录
    /// 模式，确认窗二次确认；同右键菜单项）
    pub(super) fn handle_shortcuts(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // F12 = 运行日志面板开关（诊断性质，任何模态/焦点下都可用）。
        // release 无控制台（windows 子系统），日志走 logsink 环形缓冲 +
        // archview.log 落盘；面板展示环形缓冲最近若干行。必须先于一切早退分支。
        // 三态循环：关闭 → 展开 → （收起态）→ 展开；展开时按 = 关闭。
        // 级别用 debug：面板开关属于交互噪声，info 档会刷屏
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            if self.log_open && self.log_collapsed {
                self.log_collapsed = false;
            } else {
                self.log_open = !self.log_open;
                self.log_collapsed = false;
            }
            tracing::debug!(
                "日志面板: open={} collapsed={}",
                self.log_open,
                self.log_collapsed
            );
        }

        // 模态/失焦 = 长按连发状态不可信，直接清除（None 时空转无害）：
        // 这些场景收不到 key-up——事件被编辑器/对比键表消费、搜索框接管、
        // 或窗口失焦（alt-tab）后 winit 不再送事件。用确定性信号替代旧的
        // 10s 超时兜底，事件驱动无任意时限
        if self.config_editor.is_some()
            || self.compare.is_some()
            || self.player.is_none()
            || ctx.memory(|m| m.focused()) == Some(search_box_id())
            || ctx.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::WindowFocused(false)))
            })
        {
            self.arrow_hold = None;
        }
        // config.toml 编辑器在场：键盘只归编辑器。回车/方向键/空格/字母在
        // 编辑器里都是编辑操作，全局快捷键若照常消费会抢键（实测回车误触
        // 全屏、←/→ 误触 seek）。早退后事件流不再被 retain 掉，编辑器的
        // TextEdit 原生收键，Ctrl+S / Ctrl+/ 检测也读同一事件流
        if self.config_editor.is_some() {
            return;
        }
        // 列表选择快捷键（Ctrl+A 全选 / Ctrl+Q 清除全部选中）。让位：搜索
        // 框聚焦（文本框原生收键）、密码/编码确认窗在场（它们的 TextEdit
        // 要收事件）。先于对比模态分派——列表选择与播放模式无关
        self.selection_shortcuts(ctx);
        // Ctrl+S=手动同步评级到文件（=右键菜单同项）。**必须先于对比模态
        // 分派**（2026-11-24 修复：原位置在 compare 分派短路 return 之后，
        // 对比模式 Ctrl+S 永远失联——对比模式评主栏同样会积累评级脏表，
        // 同步入口必须全局可达）。作用域区分 config 编辑器：编辑器窗口
        // 开着时 Ctrl+S 归编辑器保存（其窗口内自行匹配同款事件），这里
        // 整体跳过不抢；修饰键取 Key 事件内联快照（11_修饰键两条数据
        // 路径.md——key_pressed 依赖跨帧状态，有失焦清零坑）。防重入在
        // start_rating_sync 内部（job 在跑直接忽略）
        self.rating_sync_shortcut(ctx);
        // 对比模态：独立键表全权接管（mpv 播放键全禁，分派单位是模式）。
        // 先于"无播放器早退"——对比模式 mpv 不在场（纹理/raw 软解），无
        // 播放器也成立。**此处开始短路 return**：模态媒体键归 compare 键
        // 表，全局工具键（选择/评级同步）必须排在本分派之前
        if self.compare.is_some() {
            self.compare_shortcuts(ctx, frame);
            return;
        }
        // 视频需 mpv；图片纹理模式没有播放器也放行（翻页/缩放/全屏键可用）
        if self.player.is_none() && self.tex_image_key().is_none() {
            // 未开任何文件时 M 仍可预设静音——真相在 cfg（与控制栏 ♪ 按钮一致），
            // 播放器创建时统一应用；其余快捷键无作用对象，直接返回
            if ctx.memory(|m| m.focused()) == Some(search_box_id()) {
                return;
            }
            let mut do_mute = false;
            // C=进入多图对比：未播放/未显示图片但已打开压缩包（选中集可用）时
            // 同样成立——此前该分支只放 M，C 被早退吞掉（右键菜单不走 nav，
            // 所以菜单能进对比而 C 键无效）
            let mut enter_compare = false;
            ctx.input_mut(|i| {
                i.events.retain(|e| match e {
                    // IME 下字母键常只产 Text 事件（无 Key 事件），两种都要匹配
                    egui::Event::Text(t) if t == "m" || t == "M" => {
                        do_mute = true;
                        false
                    }
                    egui::Event::Text(t) if t == "c" || t == "C" => {
                        enter_compare = true;
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
                    egui::Event::Key {
                        key: egui::Key::C,
                        pressed: true,
                        repeat: false,
                        ..
                    } => {
                        enter_compare = true;
                        false
                    }
                    _ => true,
                });
            });
            if do_mute {
                // 播放器不在场只写 cfg（同 ♪ 按钮的未播放分支）
                self.cfg.player.muted = !self.cfg.player.muted;
            }
            if enter_compare {
                self.try_enter_compare();
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
        // 方向键长按连发：本次按下的方向（-1/+1，0=无新按下）
        let mut hold_press = 0i32;
        // 方向键释放（清除连发）
        let mut hold_release = false;
        // L=视频播放模式三态循环（定格→列表→循环）
        let mut loop_toggle = false;
        // 回车=切换全屏；Esc=退出全屏（仅全屏时消费，避免抢其它控件的 Esc）
        let mut fs_toggle = false;
        let mut fs_exit = false;
        // B=回到锚点（最后一次实际解压位置，见 App.last_focus）
        let mut go_focus = false;
        // Ctrl+H=直方图开关（当前条目为图片或 raw dump 时覆盖层有内容；
        // 开关本身无门控——未播放时按下只翻状态，覆盖层不绘制无副作用）
        let mut hist_toggle = false;
        // W/S=per-file 显示对比度步进（×1.25/×0.8，图片/raw dump/视频通吃，
        // 见 adjust_contrast）
        let (mut contrast_up, mut contrast_down) = (false, false);
        // C=进入多图对比（选中合格条目 ≥2；校验与提示在 try_enter_compare）
        let mut enter_compare = false;
        // X=快速对比（无选中时以当前播放条目为起点 + 视图序下一个合格条目，
        // 2 栏进入；校验与提示在 try_quick_compare）
        let mut quick_compare = false;
        // R=图片顺时针旋转 90°；1-5=评星、Q=清除评级（0 键已被 100% 缩放
        // 占用）。EXIF 元数据写回，见 rotate_image_cw / rate_image
        let mut rotate_cw = false;
        let mut rate_set: Option<u8> = None;
        // Ctrl+Delete=删除选中文件（目录模式；确认窗二次确认）
        let mut del_selected = false;
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
                    // C 同理：进入多图对比（无模式门控，与 Key::C 臂一致；
                    // 校验与提示在 try_enter_compare 内部）
                    if t == "c" || t == "C" {
                        enter_compare = true;
                        return false;
                    }
                    // X 同理：快速对比（无模式门控，与 Key::X 臂一致；
                    // 校验与提示在 try_quick_compare 内部）
                    if t == "x" || t == "X" {
                        quick_compare = true;
                        return false;
                    }
                    // B/N 同理：回锚点 / 下一个压缩包（无模式门控，与
                    // Key::B / Key::N 臂一致；此前仅 Key 臂可触发，IME
                    // 开启时两键失灵）
                    if t == "b" || t == "B" {
                        go_focus = true;
                        return false;
                    }
                    if t == "n" || t == "N" {
                        ask_next_key = true;
                        return false;
                    }
                    // W/S 同理：对比度步进（与 Key::W/Key::S 臂一致；
                    // adjust_contrast 无作用对象时自 early-return）
                    if t == "w" || t == "W" {
                        contrast_up = true;
                        return false;
                    }
                    if t == "s" || t == "S" {
                        contrast_down = true;
                        return false;
                    }
                    // R/Q/数字 同理：旋转/评级（仅图片模式；IME 兜底与
                    // Key 臂同语义）
                    if img_mode {
                        if t == "r" || t == "R" {
                            rotate_cw = true;
                            return false;
                        }
                        if t == "q" || t == "Q" {
                            rate_set = Some(0);
                            return false;
                        }
                        if let Ok(n) = t.parse::<u8>() {
                            if (1..=5).contains(&n) {
                                rate_set = Some(n);
                                return false;
                            }
                        }
                    }
                    return true;
                }
                // 方向键释放：清除长按连发（保留事件不消费，其它控件可读）
                if let egui::Event::Key {
                    key: egui::Key::ArrowLeft | egui::Key::ArrowRight,
                    pressed: false,
                    ..
                } = e
                {
                    hold_release = true;
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
                        // 暂停时=上一帧（逐帧），播放中=快退 5s；两种都登记长按连发
                        hold_press = -1;
                        if vid_paused {
                            frame_dir = -1;
                        } else {
                            seek_rel = -5.0;
                        }
                        false
                    }
                    egui::Key::ArrowRight => {
                        hold_press = 1;
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
                    // W/S=per-file 显示对比度步进（W=×1.25 增强、S=×0.8 降低）
                    egui::Key::W if m.is_none() => {
                        contrast_up = true;
                        false
                    }
                    egui::Key::S if m.is_none() => {
                        contrast_down = true;
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
                    // R=图片顺时针旋转 90°（EXIF Orientation 轮进，写回或会话内）
                    egui::Key::R if m.is_none() && img_mode => {
                        rotate_cw = true;
                        false
                    }
                    // Q=清除评级（0 键已被 100% 缩放占用）
                    egui::Key::Q if m.is_none() && img_mode => {
                        rate_set = Some(0);
                        false
                    }
                    // 1-5=评星（EXIF 18246/18249 写回或会话内）
                    egui::Key::Num1 if m.is_none() && img_mode => {
                        rate_set = Some(1);
                        false
                    }
                    egui::Key::Num2 if m.is_none() && img_mode => {
                        rate_set = Some(2);
                        false
                    }
                    egui::Key::Num3 if m.is_none() && img_mode => {
                        rate_set = Some(3);
                        false
                    }
                    egui::Key::Num4 if m.is_none() && img_mode => {
                        rate_set = Some(4);
                        false
                    }
                    egui::Key::Num5 if m.is_none() && img_mode => {
                        rate_set = Some(5);
                        false
                    }
                    // Ctrl+Delete=删除选中文件（确认窗二次确认；同右键菜单项）
                    egui::Key::Delete if m.ctrl => {
                        del_selected = true;
                        false
                    }
                    // C=进入多图对比视图（选中 2~6 个合格条目分栏同步缩放）
                    egui::Key::C if m.is_none() => {
                        enter_compare = true;
                        false
                    }
                    // X=快速对比（无选中：当前播放条目 + 视图序下一个合格
                    // 条目，2 栏进入；校验与提示在 try_quick_compare）
                    egui::Key::X if m.is_none() => {
                        quick_compare = true;
                        false
                    }
                    egui::Key::N if m.is_none() => {
                        ask_next_key = true;
                        false
                    }
                    // Ctrl+H=直方图开关（config 编辑器是打字密集窗口，让位）
                    egui::Key::H if m.ctrl && self.config_editor.is_none() => {
                        hist_toggle = true;
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
                self.toggle_slideshow();
            } else if let Some(p) = &self.player {
                p.toggle_pause();
            }
        }
        // L：视频播放模式三态循环（定格→列表→循环）
        if loop_toggle {
            self.cycle_play_mode();
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
        // 方向键长按连发登记：释放=清除；按下=记录方向与首发节奏锚点
        //（首拍已在上面 frame_dir/seek_rel 落地，连发从 HOLD_DELAY 后起拍）
        if hold_release {
            self.arrow_hold = None;
        }
        if hold_press != 0 {
            let now = std::time::Instant::now();
            self.arrow_hold = Some((hold_press, now + Self::ARROW_HOLD_DELAY));
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
        // C：进入多图对比视图（选中集 ∩ 入口谓词 ≥2 才放行，校验在内部）
        if enter_compare {
            self.try_enter_compare();
        }
        // X：快速对比（无选中时以当前播放为起点，校验与提示在内部）
        if quick_compare {
            self.try_quick_compare();
        }
        // Ctrl+H：直方图覆盖层开关。发起是覆盖层绘制时的按需 ensure（幂等），
        // 这里只翻状态——未播放/标准视频下翻了也不画，无副作用
        if hist_toggle {
            self.histogram_on = !self.histogram_on;
            if !self.histogram_on {
                // 关闭即清 raw 侧缓存：下次开启按当前条目重新软解，
                // 避免长会话里历史 key 的直方图驻留内存
                self.hist_raw.retain(&[]);
            }
        }
        // W/S：per-file 显示对比度步进（见 adjust_contrast）
        if contrast_up {
            self.adjust_contrast(true);
        }
        if contrast_down {
            self.adjust_contrast(false);
        }
        // R：图片顺时针旋转 90°（EXIF Orientation 写回/会话内覆盖）
        if rotate_cw {
            self.rotate_image_cw();
        }
        // 1-5/Q：评星 / 清除评级
        if let Some(r) = rate_set {
            self.rate_image(r);
        }
        // Ctrl+Delete：删除选中文件（确认窗；校验在内部）
        if del_selected {
            self.open_delete_confirm_for_selection();
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
        // 方向键长按连发驱动：每帧检查到点即发
        self.arrow_hold_fire(ctx);
    }

    // ---- per-file 显示对比度（W/S 快捷键） ----

    /// 单图模式对比度步进：W=+1 步（×1.25）、S=-1 步（×0.8）。写
    /// App.contrast_map（会话内 per-file 记忆，值 = 步数）后按媒体类型分治
    /// 下发——
    /// - 纹理图：refresh_contrast（natives 命中毫秒级重建纹理 + 直方图重算；
    ///   未命中清缓存返 false，重发 request 让 poll 上传期按新对比度落 LUT）；
    ///   步数上限 [`crate::imgview::CONTRAST_STEP_MAX`]（±6）
    /// - 视频/raw dump/mpv 回退图：mpv contrast 属性 async 写（不拿 core 锁）；
    ///   属性域 ±100 容 ±4 步（一步 = contrast ±25），步数钳 ±4 防"倍率
    ///   显示在涨画面却不再变化"
    /// - raw 对比栏不走此路径（compare_shortcuts → compare_adjust_contrast）
    pub(super) fn adjust_contrast(&mut self, up: bool) {
        let Some(key) = self
            .archive
            .as_ref()
            .and_then(|a| self.playing.map(|i| (a.id, i)))
        else {
            return;
        };
        let step = if up { 1 } else { -1 };
        let limit = if self.tex_image_key().is_some() {
            crate::imgview::CONTRAST_STEP_MAX
        } else {
            crate::imgview::CONTRAST_STEP_MAX_MPV
        };
        let cur = self.contrast_map.borrow().get(&key).copied().unwrap_or(0);
        let new = (cur + step).clamp(-limit, limit);
        self.contrast_map.borrow_mut().insert(key, new);
        self.set_notice(crate::imgview::contrast_label(new));
        if let Some(k) = self.tex_image_key() {
            let max_dim = self.cfg.image.image_max_dim;
            if !self.img.refresh_contrast(k, max_dim) {
                // natives 未命中（巨图超预算/已逐出）：refresh 已清缓存条目，
                // 按记录源立即重发——poll 上传期读 contrast_map 落 LUT
                if let Some((src_key, source, name)) = self.img_source.clone() {
                    if src_key == k {
                        self.img.request(k, source, max_dim, &name, IoClass::Decode);
                    }
                }
            }
        } else if let Some(p) = &self.player {
            p.set_display_contrast(new);
        }
    }

    /// 装载点 per-file 对比度复位：播放器按 contrast_map 中新条目的记录值对齐
    /// （player.load 内部 loadfile 后还会按 Cell 重发一次幂等对齐——mpv
    /// contrast 全局属性跨装载持续，切文件必须显式复位防上一文件对比度泄漏）
    pub(super) fn sync_player_contrast(&self, key: ImgKey) {
        let g = self.contrast_map.borrow().get(&key).copied().unwrap_or(0);
        if let Some(p) = &self.player {
            p.set_display_contrast(g);
        }
    }

    // ---- 图片方向 / 评级（EXIF 元数据写回，PLAN_image_meta.md） ----

    /// 当前显示图片条目的元数据操作上下文：(key, entry 下标, 文件路径)。
    /// 门控 = tex_image_key（排除 mpv 回退图与解码中占位）；压缩包内
    /// path = None（不可写回，评级/方向只能会话内记忆）
    fn image_meta_ctx(&self) -> Option<(ImgKey, usize, Option<PathBuf>)> {
        let key = self.tex_image_key()?;
        let a = self.archive.as_ref()?;
        let entry = a.entries.get(key.1)?;
        let path = if a.path.is_dir() {
            Some(a.path.join(&entry.name))
        } else {
            None
        };
        Some((key, key.1, path))
    }

    /// 对比模式主栏条目的元数据操作上下文（返回形态同 image_meta_ctx）。
    /// 仅静态图栏可评（PaneKind::Image）——raw 栏（nv12/yuv400/raw dump）
    /// 不在评级真相矩阵、RawErr/Unsupported 占位栏同理；None = 主栏
    /// 不存在/不可评
    pub(super) fn compare_master_meta_ctx(&self) -> Option<(ImgKey, usize, Option<PathBuf>)> {
        let s = self.compare.as_ref()?;
        let p = s.panes.get(s.master)?;
        if !matches!(p.kind, PaneKind::Image) {
            return None;
        }
        let a = self.archive.as_ref()?;
        let entry = a.entries.get(p.idx)?;
        let path = if a.path.is_dir() {
            Some(a.path.join(&entry.name))
        } else {
            None
        };
        Some((p.key, p.idx, path))
    }

    /// R 键：当前图片顺时针旋转 90°。方向真相 = 文件 EXIF Orientation
    /// tag（写回后重解码显示，位图零重写）；不可写格式（压缩包内 /
    /// GIF/BMP/写失败）降级会话内 override（rotoverride 表，request
    /// 快照下发 worker 覆盖文件 tag）。已知接受风险：invalidate 后旧
    /// 解码 worker 的迟到 Ready 可能短暂上传旧方向像素（与
    /// refresh_contrast 同款，竞态窗口毫秒级）
    fn rotate_image_cw(&mut self) {
        let Some((key, idx, path)) = self.image_meta_ctx() else {
            return;
        };
        let ename = self.archive.as_ref().unwrap().entries[idx].name.clone();
        let cur = self.img.orientation(key);
        let new = crate::meta::orientation_rotate(cur, true);
        let mut written = false;
        if let Some(p) = &path {
            if crate::meta::meta_writable(&ename) {
                match crate::meta::write_orientation(p, new) {
                    Ok(()) => {
                        // 文件 tag 已是新值：清 override 防残留抢读；
                        // 立即记录方向事实防 full 档用旧方向二次解码
                        self.img.clear_orient_override(key);
                        self.img.record_orientation(key, new);
                        written = true;
                    }
                    Err(e) => {
                        self.set_err(format!("旋转写回失败（改用会话内方向）: {e}"));
                    }
                }
            }
        }
        if !written {
            self.img.set_orient_override(key, new);
        }
        // 旋转只改 tag 不改像素，但 base 纹理/natives 是旧方向矫正产物，
        // 探针/full/对比度重建都会用过期像素 → 全清（含 natives）重解码
        self.img.invalidate_full(key);
        // 按记录源立即重发（幂等检查已被 invalidate 清缓存放行）
        if let Some((src_key, source, name)) = self.img_source.clone() {
            if src_key == key {
                let max_dim = self.cfg.image.image_max_dim;
                self.img.request(key, source, max_dim, &name, IoClass::Decode);
            }
        }
        self.set_notice(if written {
            "已顺时针旋转 90°（已写回文件）"
        } else {
            "已顺时针旋转 90°（会话内，未写回文件）"
        });
        tracing::info!("旋转 {ename}: orient {cur}→{new} written={written}");
    }

    /// 1-5/Q 键：评级（r=0 清除）。**延迟批量写**（2026-11-23 定版）：
    /// 目录模式只进 rating_dirty 内存表（路径 → 最新值），落盘 = 右键
    /// 「同步评级到文件」/ 退出静默自动保存（都带进度条）；压缩包模式
    /// 维持会话内记忆（不可写回文件）。评级真相矩阵见 meta.rs 头注
    fn rate_image(&mut self, r: u8) {
        let Some(ctx) = self.image_meta_ctx() else {
            return;
        };
        self.rate_ctx(r, ctx);
    }

    /// 评级落点（单图 image_meta_ctx / 对比模式主栏
    /// compare_master_meta_ctx 共用；r=0 清除）
    pub(super) fn rate_ctx(&mut self, r: u8, ctx: (ImgKey, usize, Option<PathBuf>)) {
        let (_, idx, path) = ctx;
        let ename = self.archive.as_ref().unwrap().entries[idx].name.clone();
        let mut dirty = false;
        if let Some(p) = &path {
            // 延迟写：同文件反复改评/清除只保留最新值（0 = 清除语义）
            self.rating_dirty.insert(p.clone(), r.min(5));
            dirty = true;
        }
        let a = self.archive.as_mut().unwrap();
        if r == 0 {
            a.ratings.remove(&idx);
        } else {
            a.ratings.insert(idx, r);
        }
        // 星级筛选生效中：评级变化可能让条目进出筛选集，重建视图；
        // 已不可见（退出筛选集）的选中项一并剔除，防旧选中被误删
        if self.rating_filter.is_some() {
            self.rebuild_view();
            let visible: std::collections::HashSet<usize> = self
                .archive
                .as_ref()
                .unwrap()
                .view
                .iter()
                .copied()
                .collect();
            self.selected.retain(|i| visible.contains(i));
            self.select_anchor = self.select_anchor.filter(|i| visible.contains(i));
        }
        let tail = if dirty { "（待同步，退出时自动保存）" } else { "（会话内）" };
        self.set_notice(if r == 0 {
            format!("已清除评级{tail}")
        } else {
            format!("已评 {}{tail}", "★".repeat(r as usize))
        });
        tracing::info!("评级 {ename}: {r} dirty={dirty}");
    }

    // ---- 方向键长按连发 ----
    // 首拍后的首发延迟（与 OS 键盘重复惯例一致）
    const ARROW_HOLD_DELAY: std::time::Duration = std::time::Duration::from_millis(300);
    // 暂停态连续逐帧：完成驱动 + 30fps 节流。下一发必须等上一发步进 seek
    // 完成（step_ready，PLAYBACK_RESTART 事件唤醒），节奏自适应 mpv 逐帧消化
    // 速度、无丢命令错拍；33ms 为最快上限（≈30fps，逐帧快过 30fps 肉眼无益）
    const ARROW_HOLD_STEP: std::time::Duration = std::time::Duration::from_millis(33);
    // 完成闸门的兜底强发超时：步进 seek 异常无 RESTART（如 EOF 尾帧）时不
    // 让连发永久卡死
    const ARROW_HOLD_STALL: std::time::Duration = std::time::Duration::from_millis(400);
    // 播放态连续 ±5s seek 间隔（≈33s/s；mpv 对 RELATIVE seek 入队累加合并）
    const ARROW_HOLD_SEEK: std::time::Duration = std::time::Duration::from_millis(150);

    /// 方向键长按连发：暂停=连续逐帧步进，播放=连续 ±5s seek，松开即停。
    /// 每帧由 handle_shortcuts 尾部驱动。切方向=重置节奏锚点，换挡（暂停↔播放）
    /// 在每次 fire 时实时读 paused() 缓存自适应。按住失效场景（失焦/模态）
    /// 由 handle_shortcuts 头部的清除逻辑兜底，无任意时限
    fn arrow_hold_fire(&mut self, ctx: &egui::Context) {
        let Some((dir, next_fire)) = self.arrow_hold else {
            return;
        };
        let now = std::time::Instant::now();
        if now < next_fire {
            ctx.request_repaint_after(next_fire - now);
            return;
        }
        let paused = self.player.as_ref().is_some_and(|p| p.paused());
        if paused {
            // 逐帧：完成驱动。上一发步进 seek 未完成（step_ready=false）时
            // 等 PLAYBACK_RESTART 唤醒（泵线程 request_repaint），双保险再排
            // 一个兜底轮询；异常卡死由 STALL 超时强发兜底
            let ready = self.player.as_ref().is_some_and(|p| p.step_ready());
            if !ready && now - next_fire < Self::ARROW_HOLD_STALL {
                ctx.request_repaint_after(std::time::Duration::from_millis(25));
                return;
            }
            self.arrow_hold = Some((dir, now + Self::ARROW_HOLD_STEP));
            // 正常由完成事件唤醒下一发；兜底排帧保证闸门永不失联
            ctx.request_repaint_after(Self::ARROW_HOLD_STEP);
            if let Some(p) = &self.player {
                p.frame_step(dir);
            }
            return;
        }
        self.arrow_hold = Some((dir, now + Self::ARROW_HOLD_SEEK));
        // 播放态 vsync 帧率更密，request_repaint_after 与之取较早者，无害
        ctx.request_repaint_after(Self::ARROW_HOLD_SEEK);
        if let Some(p) = &self.player {
            p.seek_relative(dir as f64 * 5.0);
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

    /// 图片自动翻页开关：**空格快捷键与控制栏 ▶/|| 中键共用同一动作**
    /// （同状态两入口不漂移；对齐对比模式 compare_toggle_autoplay 先例——
    /// 此前两处各自内联翻转同一字段，语义一致但代码路径分叉）
    pub(super) fn toggle_slideshow(&mut self) {
        self.slideshow_on = !self.slideshow_on;
    }

    /// 图片自动播放（PPT 式翻页）：间隔到了切下一张；手动切换会重置计时。
    /// 到期翻页由精确排帧唤醒（mod.rs 心跳决策 `request_repaint_after
    /// (slideshow_rest)`），无需周期心跳轮询
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

    /// 距下次幻灯片翻页的剩余时长（saturating；UI 心跳决策据此精确排帧，
    /// 每个翻页间隔只醒一次而非 10 FPS 轮询）。`now` = ctx.input time（秒，
    /// 与 slideshow_tick 计时同源同帧同值）
    pub(super) fn slideshow_rest(&self, now: f64) -> std::time::Duration {
        let rest = (self.cfg.ui.slideshow_secs - (now - self.slideshow_anchor)).max(0.0);
        // 下限 1ms 防零时长忙转；上限防病态配置撑爆 Duration
        std::time::Duration::from_millis(((rest * 1000.0) as u64).clamp(1, 3_600_000))
    }

    /// 循环切换视频播放模式（三态）：Hold（定格，默认，播完停末帧）→
    /// List（列表，播完自动下一个）→ Loop（mpv loop-file=inf 原生循环，
    /// EOF 不触发）→ Hold。切到 Loop 时若已停在末帧（keep-open EOF
    /// 粘滞暂停），回开头复活继续；Hold→List 若正粘滞末帧，下一帧
    /// eof_tick 立即推进（Hold 门控在读取 eof 之前 return，不写
    /// eof_handled；列表末尾停住的推进已记 eof_handled，不会误推进）
    pub(super) fn cycle_play_mode(&mut self) {
        self.play_mode = match self.play_mode {
            PlayMode::Hold => PlayMode::List,
            PlayMode::List => PlayMode::Loop,
            PlayMode::Loop => PlayMode::Hold,
        };
        let m = self.play_mode;
        if let Some(p) = &self.player {
            p.set_loop(m == PlayMode::Loop);
            if m == PlayMode::Loop && p.eof_reached() {
                // loop-file 不回溯已发生的 EOF：手动回开头并解除粘滞暂停
                p.seek(0.0);
                p.set_paused(false);
            }
        }
        self.set_notice(match m {
            PlayMode::Hold => "单文件播放（播完停在末帧）",
            PlayMode::List => "列表播放（播完自动下一个）",
            PlayMode::Loop => "单视频循环",
        });
        tracing::info!("视频播放模式: {m:?}");
    }

    /// 列表播放：视频播到末尾自动切下一个（事件驱动：mpv 泵线程在
    /// eof-reached 置位时 request_repaint 唤醒一帧，本 tick 在该帧内推进，
    /// 延迟为事件到达即帧，不可感知）。
    /// 仅 List 模式推进；Loop 下 eof-reached 恒 false（mpv 原生循环不
    /// 触发 EOF）；Hold 下 eof 可为 true 但不推进——切回 List 时立即
    /// 推进（Hold 门控在读取 eof 之前 return，不写 eof_handled）。
    /// eof_handled 防重入：推进无效果时（列表末尾停住/目标需先解压等
    /// 装载）不每帧重复触发；eof 复位（新文件开播/手动回拖）自动清空
    pub(super) fn video_eof_tick(&mut self) {
        if self.play_mode != PlayMode::List || self.playing.is_none() || self.playing_is_image() {
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
