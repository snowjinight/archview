//! egui UI：左侧文件列表+搜索，右侧播放区（mpv 子窗口嵌入）+ 底部控制栏。

use crate::cache;
use crate::config::Config;
use crate::player::stream_cb;
use crate::player::PlayerService;
use crate::vfs::{open_archive, AccessMode, ArchiveVFS, VirtualEntry, VfsError};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

/// 搜索框固定 Id：快捷键处理时仅对该控件让出键盘
fn search_box_id() -> egui::Id {
    egui::Id::new("search_box")
}
/// 进度条/播放头主题色
const ACCENT: egui::Color32 = egui::Color32::from_rgb(64, 156, 255);

/// 已打开的压缩包状态
struct ArchiveState {
    id: u64,
    /// 压缩包文件名（显示用）
    name: String,
    /// 压缩包路径（带密码重开用）
    path: PathBuf,
    /// 本包密码（内容加密条目解密用；仅存内存，切换包/退出即丢）
    password: Option<String>,
    /// 全部 entry
    entries: Vec<VirtualEntry>,
    /// 搜索过滤后的 entry 下标（display 顺序）
    view: Vec<usize>,
    /// VFS 引用（后台解压线程用）
    vfs: Arc<dyn ArchiveVFS>,
    /// true = 流式播放（zip 快路径）；false = 后台解压（7z/RAR 慢路径）
    stream: bool,
}

/// 密码输入弹窗：加密头（打开阶段）或内容加密（播放阶段）触发
struct PwPrompt {
    kind: PwKind,
    input: String,
    /// true = 上次提交的密码被拒绝（显示"密码错误"）
    wrong: bool,
}

#[derive(Clone)]
enum PwKind {
    /// 打开压缩包需要密码（加密头），提交成功后完成打开
    Open(PathBuf),
    /// 播放加密条目需要密码，提交成功后重建 VFS 并重试播放
    Play(usize),
}

/// 后台解压任务（7z/RAR）
struct ExtractJob {
    gen: u64,
    entry_idx: usize,
    dest: PathBuf,
    entry_name: String,
    total: u64,
    done: u64,
    rx: Receiver<ExtMsg>,
}

enum ExtMsg {
    Progress(u64),
    Done,
    /// (错误信息, 是否密码问题)：密码问题弹密码框重试，其它报错
    Failed(String, bool),
}

/// 文件列表标签页
#[derive(Clone, Copy, PartialEq)]
enum ListTab {
    Video,
    Image,
}

pub struct App {
    cfg: Config,
    archive: Option<ArchiveState>,
    search: String,
    /// 当前播放的 entry（entries 下标）
    playing: Option<usize>,
    /// mpv 播放子窗口 HWND（0 = 未创建）
    child_hwnd: isize,
    // 子窗口当前是否因弹窗遮挡而临时隐藏
    child_hidden: bool,
    // 本帧是否有弹窗打开（帮助/密码框）：mpv 子窗口永远盖在 egui 之上，
    // 任何弹窗显示期间需隐藏子窗口，帧末统一同步（每帧开头重置）
    overlay_open: bool,
    player: Option<PlayerService>,
    // 进度条拖动状态
    dragging: bool,
    drag_pos: f64,
    error: Option<String>,
    // 一次性提示（如缓存清理结果），显示在顶栏
    notice: Option<String>,
    // 全屏播放中（隐藏所有面板，仅视频+进度条）
    fullscreen: bool,
    // 文件列表当前标签页（视频/图片）
    list_tab: ListTab,
    // 高亮滚动跟踪：已滚动到的 playing（变化时才触发 scroll_to_me）
    scrolled_to: Option<usize>,
    // 滚轮切换：上次切换时刻（0.5s 内的滚轮操作忽略，防连跳）
    wheel_last: f64,
    // 图片自动播放（PPT 式翻页）：开关 + 计时锚点（playing 变化时重置）
    slideshow_on: bool,
    slideshow_idx: Option<usize>,
    slideshow_anchor: f64,
    // 后台解压（7z/RAR）
    extract: Option<ExtractJob>,
    next_gen: u64,
    // 密码输入弹窗（加密压缩包）
    pw_prompt: Option<PwPrompt>,
    // 会话内密码记忆：压缩包路径 → 密码（内存中，退出即丢；切包回来不用重输）
    pw_memory: std::collections::HashMap<PathBuf, String>,
}

impl App {
    pub fn new(cfg: Config, cc: &eframe::CreationContext) -> Self {
        Self::install_cjk_font(&cfg, &cc.egui_ctx);
        // 密码候选代码页（config: password_codepage，默认 gbk）
        crate::vfs::set_password_codepage(&cfg.password_codepage);
        // 清理上次中断解压留下的 .part 半截文件
        cache::cleanup_partial(&cache::cache_dir(&cfg));
        Self {
            cfg,
            archive: None,
            search: String::new(),
            playing: None,
            child_hwnd: 0,
            child_hidden: false,
            overlay_open: false,
            player: None,
            dragging: false,
            drag_pos: 0.0,
            error: None,
            notice: None,
            fullscreen: false,
            list_tab: ListTab::Video,
            scrolled_to: None,
            wheel_last: f64::NEG_INFINITY,
            slideshow_on: false,
            slideshow_idx: None,
            slideshow_anchor: 0.0,
            extract: None,
            next_gen: 0,
            pw_prompt: None,
            pw_memory: std::collections::HashMap::new(),
        }
    }

    /// 把 CJK 字体追加进 egui 字体族（中文/日文等不再显示方块）。
    /// 字体路径来自 config.json（"auto" = 系统字体目录候选），找不到则回退默认字体。
    fn install_cjk_font(cfg: &Config, ctx: &egui::Context) {
        let Some(font_path) = cfg.cjk_font_path() else {
            tracing::warn!("未找到 CJK 字体，非拉丁字符将显示为方块（可在 config.json 配 ui.font_path）");
            return;
        };
        match std::fs::read(&font_path) {
            Ok(bytes) => {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "cjk".into(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                // 追加到默认字体之后：拉丁字符仍用内置字体，CJK 用补充字体
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts.families.entry(family).or_default().push("cjk".into());
                }
                ctx.set_fonts(fonts);
                tracing::info!("已加载 CJK 字体: {}", font_path.display());
            }
            Err(e) => tracing::warn!("读取字体 {} 失败: {e}", font_path.display()),
        }
    }

    fn set_err(&mut self, e: impl Into<String>) {
        let e = e.into();
        tracing::error!("{e}");
        self.error = Some(e);
    }

    // ---- 打开压缩包 ----

    fn open_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Archives", &["zip", "7z", "rar"])
            .pick_file()
        else {
            return;
        };
        self.open_path(path);
    }

    /// 处理本帧拖入的文件（egui dropped_files）：取第一个 .zip 打开
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path().to_path_buf().into())
                .collect()
        });
        if let Some(path) = dropped.into_iter().next() {
            self.open_path(path);
        }
    }

    fn open_path(&mut self, path: std::path::PathBuf) {
        // 会话内密码记忆：本会话输过密码的包直接带密码打开（不再弹框）
        let cached = self.pw_memory.get(&path).cloned();
        self.open_path_pw(path, cached.as_deref());
    }

    /// 打开压缩包；Password 错误（加密头）时弹密码框，提交后带密码重开
    fn open_path_pw(&mut self, path: std::path::PathBuf, password: Option<&str>) {
        match open_archive(&path, password) {
            Ok(vfs) => {
                // 切换压缩包：按配置清理上一个包的解压缓存（默认不清理——保留可命中）
                if self.cfg.cache.clean_on_switch {
                    if let Some(old) = &self.archive {
                        let freed = cache::clean_archive(&cache::cache_dir(&self.cfg), old.id);
                        if freed > 0 {
                            tracing::info!("切换压缩包，清理旧缓存 {:.1} MB", freed as f64 / 1048576.0);
                        }
                    }
                }
                let arc: Arc<dyn ArchiveVFS> = vfs.into();
                let stream = arc.access_mode() == AccessMode::FastSeek;
                // 稳定 ID（路径哈希，register 内防哈希碰撞）：A→B→A 重开时缓存路径不变，可命中
                let (id, entries) = stream_cb::register(arc.clone(), &path);
                // 密码记忆：带密码成功打开的包记入会话表（切回来不用重输）
                if let Some(pw) = password {
                    self.pw_memory.insert(path.clone(), pw.to_string());
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.archive = Some(ArchiveState {
                    id,
                    name,
                    path: path.clone(),
                    password: password.map(str::to_string),
                    entries,
                    view: Vec::new(),
                    vfs: arc,
                    stream,
                });
                self.rebuild_view();
                self.playing = None;
                self.error = None;
                self.pw_prompt = None;
            }
            // 加密头（7z/RAR 加密文件名）：弹密码框重开
            Err(VfsError::Password) => {
                self.pw_prompt = Some(PwPrompt {
                    kind: PwKind::Open(path),
                    input: String::new(),
                    wrong: password.is_some(), // 带密码仍失败 = 密码错误
                });
            }
            Err(e) => self.set_err(format!("Failed to open {}: {e}", path.display())),
        }
    }

    /// 重新计算过滤视图：搜索框子串匹配 + 只显示媒体文件（视频/图片按 config 分类），
    /// 按文件名不区分大小写排序（保证序号/上一个下一个顺序稳定，与归档内部顺序无关）
    fn rebuild_view(&mut self) {
        let Some(a) = &mut self.archive else { return };
        let search = self.search.to_lowercase();
        a.view = a
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                !e.is_dir
                    && (self.cfg.is_video_ext(&e.name) || self.cfg.is_image_ext(&e.name))
                    && (search.is_empty() || e.name.to_lowercase().contains(&search))
            })
            .map(|(i, _)| i)
            .collect();
        let entries = &a.entries;
        a.view.sort_by(|&x, &y| {
            entries[x]
                .name
                .to_lowercase()
                .cmp(&entries[y].name.to_lowercase())
        });
    }

    // ---- 播放 ----

    /// 确保 mpv 播放器与子窗口就绪；返回 false 时已设置错误
    fn ensure_player(&mut self, frame: &mut eframe::Frame) -> bool {
        if self.player.is_none() {
            match PlayerService::new(&self.cfg) {
                Ok(p) => self.player = Some(p),
                Err(e) => {
                    self.set_err(e);
                    return false;
                }
            }
        }
        if self.child_hwnd == 0 {
            match native_hwnd(frame) {
                Some(parent) => self.create_child(parent),
                None => {
                    self.set_err("Failed to get native window handle");
                    return false;
                }
            }
            if self.child_hwnd == 0 {
                self.set_err("Failed to create player child window");
                return false;
            }
            if let Some(p) = &self.player {
                p.embed(self.child_hwnd);
            }
        }
        true
    }

    fn play(&mut self, entry_idx: usize, frame: &mut eframe::Frame) {
        // 先读模式标记结束借用，再 ensure_player（需要 &mut self）
        let Some(stream) = self.archive.as_ref().map(|a| a.stream) else {
            return;
        };
        // 两条路径都需要播放器与子窗口，先就绪
        if !self.ensure_player(frame) {
            return;
        }
        if stream {
            // 快路径：流式播放（zvf://）
            let Some(arch) = &self.archive else { return };
            // 内容加密条目（zip AES/ZipCrypto）：无密码先弹密码框，带密码重建 VFS 后再播
            if arch.entries[entry_idx].encrypted && arch.password.is_none() {
                self.pw_prompt = Some(PwPrompt {
                    kind: PwKind::Play(entry_idx),
                    input: String::new(),
                    wrong: false,
                });
                return;
            }
            let (archive_id, name) = (arch.id, arch.entries[entry_idx].name.clone());
            if let Some(p) = &self.player {
                p.load(archive_id, entry_idx);
            }
            self.playing = Some(entry_idx);
            self.dragging = false;
            tracing::info!("播放: {name}");
        } else {
            // 慢路径：后台解压（7z/RAR 固态）
            // 已知加密且无密码：直接弹密码框（RAR 加密与否在解压时才暴露，仍走线程反馈）
            let need_pw = self.archive.as_ref().is_some_and(|a| {
                a.entries[entry_idx].encrypted && a.password.is_none()
            });
            if need_pw {
                self.pw_prompt = Some(PwPrompt {
                    kind: PwKind::Play(entry_idx),
                    input: String::new(),
                    wrong: false,
                });
                return;
            }
            self.start_extract(entry_idx);
        }
    }

    /// 启动后台解压任务；缓存命中（同尺寸文件已在）则直接播放
    fn start_extract(&mut self, entry_idx: usize) {
        // 去重：该条目的解压线程已在跑（双击/重复点击会瞬时触发两次 play），
        // 不再起第二个线程——两个 unrar 抢写同一个临时文件会报 "Could not create file"。
        // 在跑的任务完成后会自动播放（gen 未变）。
        if self
            .extract
            .as_ref()
            .is_some_and(|j| j.entry_idx == entry_idx)
        {
            return;
        }
        let Some(arch) = &self.archive else { return };
        let entry = arch.entries[entry_idx].clone();
        let vfs = arch.vfs.clone();
        let dir = cache::cache_dir(&self.cfg);
        let dest = cache::dest_path(&dir, arch.id, entry_idx, &entry.name);

        // 缓存命中：尺寸一致直接播
        if let Ok(meta) = std::fs::metadata(&dest) {
            if meta.len() == entry.size {
                tracing::info!("缓存命中: {}", dest.display());
                self.extract = None;
                self.load_extracted(&dest, entry_idx);
                return;
            }
        }

        cache::evict_if_needed(
            &dir,
            self.cfg.cache.max_size_mb * 1024 * 1024,
            entry.size,
        );

        self.next_gen += 1;
        let gen = self.next_gen;
        let (tx, rx) = mpsc::channel::<ExtMsg>();
        let name = entry.name.clone();
        let total = entry.size;
        let dest_for_thread = dest.clone();
        std::thread::Builder::new()
            .name("extract".into())
            .spawn(move || {
                let r = vfs.extract_entry(&entry, &dest_for_thread, &|done| {
                    let _ = tx.send(ExtMsg::Progress(done));
                });
                match r {
                    Ok(()) => {
                        let _ = tx.send(ExtMsg::Done);
                    }
                    Err(e) => {
                        let is_pwd = matches!(e, VfsError::Password);
                        let _ = tx.send(ExtMsg::Failed(e.to_string(), is_pwd));
                    }
                }
            })
            .ok();

        tracing::info!("后台解压: {name} → {}", dest.display());
        self.extract = Some(ExtractJob {
            gen,
            entry_idx,
            dest,
            entry_name: name,
            total,
            done: 0,
            rx,
        });
    }

    /// 每帧轮询解压任务状态
    fn poll_extract(&mut self) {
        loop {
            let Some(job) = &mut self.extract else { return };
            let msg = match job.rx.try_recv() {
                Ok(m) => m,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    ExtMsg::Failed(
                        format!("解压线程中断（条目 [{}]）", job.entry_name),
                        false,
                    )
                }
            };
            let (gen, entry_idx, dest, entry_name) = (
                job.gen,
                job.entry_idx,
                job.dest.clone(),
                job.entry_name.clone(),
            );
            match msg {
                ExtMsg::Progress(done) => {
                    if let Some(j) = &mut self.extract {
                        j.done = done;
                    }
                    return; // 本帧处理完毕，下帧继续
                }
                ExtMsg::Done => {
                    self.extract = None;
                    // gen 匹配才播放（用户可能已切换选择）
                    if gen == self.next_gen {
                        self.load_extracted(&dest, entry_idx);
                    }
                }
                ExtMsg::Failed(e, is_pwd) => {
                    self.extract = None;
                    if is_pwd {
                        // 内容加密条目（7z/RAR）：弹密码框，提交后重建 VFS 重试
                        self.pw_prompt = Some(PwPrompt {
                            kind: PwKind::Play(entry_idx),
                            input: String::new(),
                            wrong: true,
                        });
                    } else {
                        // 日志带完整上下文（压缩包/条目/目标路径）；
                        // 用户可见错误直接展示 VFS 层消息（其中已含条目与路径细节）
                        let arch = self
                            .archive
                            .as_ref()
                            .map(|a| a.name.as_str())
                            .unwrap_or("<未知压缩包>");
                        tracing::error!(
                            "解压失败: 压缩包 [{arch}] 条目 [{entry_name}] → [{}]: {e}",
                            dest.display()
                        );
                        self.error = Some(format!("解压失败: {e}"));
                    }
                }
            }
        }
    }

    /// 密码提交后：带密码重开压缩包（重建 VFS）并重试播放指定 entry
    fn replay_with_password(&mut self, entry_idx: usize, pw: String, frame: &mut eframe::Frame) {
        let Some(arch) = self.archive.as_ref() else { return };
        let (path, name, stream) = (arch.path.clone(), arch.name.clone(), arch.stream);
        let entry = arch.entries[entry_idx].clone();
        match open_archive(&path, Some(&pw)) {
            Ok(vfs) => {
                let arc: Arc<dyn ArchiveVFS> = vfs.into();
                // zip 流式：open_entry 打开时即校验密码（AES），错密码不替换状态、直接重弹
                if stream {
                    if let Err(VfsError::Password) = arc.open_entry(&entry) {
                        self.pw_prompt = Some(PwPrompt {
                            kind: PwKind::Play(entry_idx),
                            input: String::new(),
                            wrong: true,
                        });
                        return;
                    }
                }
                let (id, entries) = stream_cb::register(arc.clone(), &path);
                // 密码以新输入为准更新记忆（包被替换密码时旧记忆失效）
                self.pw_memory.insert(path.clone(), pw.clone());
                self.archive = Some(ArchiveState {
                    id,
                    name,
                    path,
                    password: Some(pw),
                    entries,
                    view: Vec::new(),
                    vfs: arc,
                    stream,
                });
                self.rebuild_view();
                self.error = None;
                self.playing = None;
                self.play(entry_idx, frame);
            }
            Err(VfsError::Password) => {
                self.pw_prompt = Some(PwPrompt {
                    kind: PwKind::Play(entry_idx),
                    input: String::new(),
                    wrong: true,
                });
            }
            Err(e) => self.set_err(format!("重新打开失败: {e}")),
        }
    }

    /// 密码输入弹窗（加密头=打开阶段 / 加密条目=播放阶段）。
    /// mpv 原生子窗口永远盖在 egui 绘制之上，窗口固定在左侧文件列表区避开视频区
    fn password_dialog(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let Some(p) = self.pw_prompt.as_mut() else { return };
        // 密码框显示期间隐藏 mpv 子窗口（与帮助弹窗同机制，帧末统一同步）；
        // 原先 fixed_pos 避让方案在窗口缩放/高 DPI 下仍可能被视频挡住
        self.overlay_open = true;
        let mut submitted = false;
        let mut cancelled = false;
        let title = match &p.kind {
            PwKind::Open(_) => "压缩包已加密",
            PwKind::Play(_) => "文件已加密",
        };
        egui::Window::new(title)
            .fixed_pos((60.0, 140.0))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(300.0);
                if p.wrong {
                    ui.colored_label(egui::Color32::RED, "密码错误，请重试");
                } else {
                    ui.label(match &p.kind {
                        PwKind::Open(_) => "此压缩包的文件名已加密，需要密码才能打开",
                        PwKind::Play(_) => "此文件已加密，需要密码才能播放",
                    });
                }
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut p.input)
                        .password(true)
                        .hint_text("密码"),
                );
                // 回车提交
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submitted = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("确定").clicked() {
                        submitted = true;
                    }
                    if ui.button("取消").clicked() {
                        cancelled = true;
                    }
                });
            });
        if submitted {
            let prompt = self.pw_prompt.take().unwrap();
            match prompt.kind {
                PwKind::Open(path) => self.open_path_pw(path, Some(&prompt.input)),
                PwKind::Play(idx) => self.replay_with_password(idx, prompt.input, frame),
            }
        } else if cancelled {
            self.pw_prompt = None;
        }
    }

    /// 播放已解压到本地的文件
    fn load_extracted(&mut self, dest: &Path, entry_idx: usize) {
        if let Some(p) = &self.player {
            p.load_path(dest);
        }
        self.playing = Some(entry_idx);
        self.dragging = false;
    }

    fn create_child(&mut self, parent: isize) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_TRANSPARENT, WS_VISIBLE,
        };
        const CLASS: &[u16] = &[83, 84, 65, 84, 73, 67, 0]; // "STATIC\0"
        // WS_EX_TRANSPARENT：鼠标事件穿透回 egui 宿主窗口——
        // 否则整个视频区（子窗口覆盖处）的滚轮/双击/悬停全部被子窗口吞掉
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TRANSPARENT,
                CLASS.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                0,
                0,
                100,
                100,
                parent as _,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        self.child_hwnd = hwnd as isize;
    }

    /// 每帧同步子窗口矩形到 egui 播放区（物理像素坐标）。
    /// egui 矩形是逻辑点，SetWindowPos 要物理像素：x/y/w/h 必须全部乘 ppp，
    /// 否则高分屏（如 150% 缩放）下子窗口会向左上偏移、侵入文件列表
    fn sync_child_rect(&self, rect: egui::Rect, ppp: f32) {
        if self.child_hwnd == 0 {
            return;
        }
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
        };
        let x = (rect.left() * ppp) as i32;
        let y = (rect.top() * ppp) as i32;
        let w = (rect.width() * ppp) as i32;
        let h = (rect.height() * ppp) as i32;
        unsafe {
            SetWindowPos(
                self.child_hwnd as _,
                std::ptr::null_mut(),
                x,
                y,
                w.max(1),
                h.max(1),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    // ---- 快捷键（参考 mpv 习惯） ----

    /// 全屏切换（双击画面 / F11），全屏时隐藏所有面板只留视频+进度条
    fn set_fullscreen(&mut self, on: bool, frame: &mut eframe::Frame) {
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

    /// 空格=暂停/播放，←/→=±5s，Shift+←/→=±1s，↑/↓=音量±5，M=静音，PgUp/PgDn=上一个/下一个，
    /// 回车=全屏切换，Esc=退出全屏
    fn handle_shortcuts(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.player.is_none() {
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
                let egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } = e
                else {
                    return true;
                };
                let m = *modifiers;
                match key {
                    egui::Key::Space if m.is_none() => {
                        do_pause = true;
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
            self.play_adjacent(frame, step_entry);
        }
        if fs_toggle {
            self.set_fullscreen(!self.fullscreen, frame);
        }
        if fs_exit {
            self.set_fullscreen(false, frame);
        }
    }

    /// 当前播放的条目是否是图片
    fn playing_is_image(&self) -> bool {
        self.archive
            .as_ref()
            .and_then(|a| self.playing.map(|i| &a.entries[i]))
            .map(|e| self.cfg.is_image_ext(&e.name))
            .unwrap_or(false)
    }

    /// 播放列表中的上一个/下一个媒体（offset = -1/+1）。
    /// 只在当前条目同类型内导航（视频↔视频，图片↔图片），避免视频播着切到图片。
    /// 未播放时：按当前标签页类型从第一项（下一个）/最后一项（上一个）开始。
    fn play_adjacent(&mut self, frame: &mut eframe::Frame, offset: i32) {
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
            if np < 0 || (np as usize) >= same_kind.len() {
                return;
            }
            same_kind[np as usize]
        };
        self.play(ei, frame);
    }

    /// 图片自动播放（PPT 式翻页）：间隔到了切下一张；手动切换会重置计时
    fn slideshow_tick(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
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
            self.play_adjacent(frame, 1);
        }
    }

    // ---- 各面板 ----

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open Archive…").clicked() {
                self.open_dialog();
            }
            let arch_name = self
                .archive
                .as_ref()
                .map(|a| format!("{} ({} media items)", a.name, a.view.len()))
                .unwrap_or_else(|| "No archive".into());
            ui.label(arch_name);
            // 打开缓存目录（系统文件管理器；缓存目录本身不存在则先创建）
            if ui.button("缓存目录").clicked() {
                let dir = cache::cache_dir(&self.cfg);
                let _ = std::fs::create_dir_all(&dir);
                // explorer 打开目录不占用等待（spawn 后立刻返回）
                if let Err(e) = std::process::Command::new("explorer").arg(&dir).spawn() {
                    self.notice = Some(format!("打开缓存目录失败: {e}"));
                }
            }
            if ui.button("清理缓存").clicked() {
                let freed = cache::clean_all(&cache::cache_dir(&self.cfg));
                self.notice = Some(format!("已清理缓存 {:.1} MB", freed as f64 / 1048576.0));
                tracing::info!("手动清理缓存: {:.1} MB", freed as f64 / 1048576.0);
            }
            // 快捷键帮助（弹出面板，不占常驻空间；egui 0.36 用 from_toggle_button_response 自管开关）
            let help_btn = ui.button("快捷键 ?");
            let help_open = egui::Popup::from_toggle_button_response(&help_btn)
                .show(|ui| {
                    ui.set_min_width(280.0);
                    ui.strong("快捷键");
                    ui.separator();
                    for (k, v) in [
                        ("空格", "暂停 / 播放（图片=自动翻页开关）"),
                        ("← / →", "后退 / 前进 5 秒"),
                        ("Shift + ← / →", "后退 / 前进 1 秒"),
                        ("↑ / ↓", "音量 ±5"),
                        ("M", "静音切换"),
                        ("PgUp / PgDn", "上一个 / 下一个"),
                        ("鼠标滚轮（视频区）", "上一个 / 下一个"),
                        ("回车 / 双击画面", "全屏切换"),
                        ("Esc", "退出全屏"),
                    ] {
                        ui.horizontal(|ui| {
                            ui.strong(k);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(v);
                            });
                        });
                    }
                })
                .is_some();
            // mpv 原生子窗口永远盖在 egui 之上：帮助面板打开期间临时隐藏子窗口
            // （置位 overlay_open，帧末统一同步 ShowWindow，与密码框共用机制）
            if help_open {
                self.overlay_open = true;
            }
            if let Some(n) = &self.notice {
                ui.colored_label(egui::Color32::LIGHT_GREEN, n);
            }
            if let Some(e) = &self.error {
                ui.colored_label(egui::Color32::RED, e);
            }
            let buffering = self
                .player
                .as_ref()
                .map(|p| p.buffering())
                .unwrap_or(false);
            if buffering {
                ui.colored_label(egui::Color32::YELLOW, "Buffering…");
            }
            // 后台解压状态（7z/RAR）
            if let Some(j) = &self.extract {
                let mb = |b: u64| format!("{:.1} MB", b as f64 / 1024.0 / 1024.0);
                let text = if j.total > 0 {
                    format!("Extracting {}… {}/{}", j.entry_name, mb(j.done), mb(j.total))
                } else {
                    format!("Extracting {}…", j.entry_name)
                };
                ui.colored_label(egui::Color32::YELLOW, text);
            }
        });
    }

    fn file_list(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
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

        // 预收集当前标签页数据，避免渲染闭包内借用 self
        struct Row {
            idx: usize,
            no: usize, // 当前标签页内序号（1 起）
            label: String,
            selected: bool,
        }
        let mut rows: Vec<Row> = Vec::new();
        let mut counts = (0usize, 0usize); // (视频, 图片)
        if let Some(a) = &self.archive {
            for &ei in &a.view {
                let e = &a.entries[ei];
                let is_video = self.cfg.is_video_ext(&e.name);
                if is_video {
                    counts.0 += 1;
                } else {
                    counts.1 += 1;
                }
                // 只收集当前标签页的行
                if (self.list_tab == ListTab::Video) != is_video {
                    continue;
                }
                rows.push(Row {
                    idx: ei,
                    no: rows.len() + 1,
                    label: e.name.clone(),
                    selected: self.playing == Some(ei),
                });
            }
        }

        // 标签页切换（视频 N | 图片 M）
        ui.horizontal(|ui| {
            let tab = |ui: &mut egui::Ui, cur: ListTab, want: ListTab, title: &str| {
                let text = if cur == want {
                    egui::RichText::new(title).strong()
                } else {
                    egui::RichText::new(title).weak()
                };
                ui.selectable_label(cur == want, text).clicked()
            };
            if tab(ui, self.list_tab, ListTab::Video, &format!("视频 ({})", counts.0)) {
                self.list_tab = ListTab::Video;
            }
            if tab(ui, self.list_tab, ListTab::Image, &format!("图片 ({})", counts.1)) {
                self.list_tab = ListTab::Image;
            }
        });
        ui.separator();

        let mut action: Option<usize> = None;
        let mut scrolled = false;
        // auto_shrink=false：滚动区域宽度固定占满面板，滚动条钉在右边界（不随文件名长度缩进）
        egui::ScrollArea::vertical()
            .auto_shrink(egui::Vec2b::new(false, false))
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.weak(if self.archive.is_some() {
                        "（无匹配媒体文件）"
                    } else {
                        "打开压缩包后文件列表显示在这里"
                    });
                }
                for r in &rows {
                    let label = format!("[{}/{}] {}", r.no, rows.len(), r.label);
                    let resp = ui
                        .selectable_label(r.selected, label)
                        .on_hover_text(&r.label);
                    // 高亮跟踪：playing 变化（快捷键/箭头切换）时自动滚动到该行（居中）
                    if r.selected && self.scrolled_to != self.playing {
                        resp.scroll_to_me(Some(egui::Align::Center));
                        scrolled = true;
                    }
                    if resp.clicked() {
                        action = Some(r.idx);
                    }
                }
            });
        if scrolled {
            self.scrolled_to = self.playing;
        }

        if let Some(ei) = action {
            self.play(ei, frame);
        }
    }

    fn video_area(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // mpv 子窗口占满整个中央面板（子窗口带 WS_EX_TRANSPARENT，鼠标事件穿透回 egui）
        let video = ui.available_rect_before_wrap();
        let child = video;

        // 视频区（原生子窗口占位）；双击=全屏（单击暂停已取消：与双击冲突，改控制栏按钮）
        let resp = ui.allocate_rect(video, egui::Sense::click());
        if self.playing.is_none() || self.child_hwnd == 0 {
            let text = if self.archive.is_some() {
                "点击左侧视频文件开始播放"
            } else {
                "先打开一个压缩包（ZIP / 7Z / RAR）"
            };
            ui.put(
                egui::Rect::from_center_size(child.center(), egui::vec2(400.0, 30.0)),
                egui::Label::new(egui::RichText::new(text).weak()).selectable(false),
            );
        }
        if resp.double_clicked() {
            self.set_fullscreen(!self.fullscreen, frame);
        }

        // 滚轮=上一个/下一个（悬停视频区时）：检测到滚动即切换一次，
        // 间隔内（config: ui.wheel_switch_secs）的后续滚轮操作忽略（防惯性/触控板连跳）
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                let now = ui.input(|i| i.time);
                if now - self.wheel_last >= self.cfg.ui.wheel_switch_secs {
                    self.wheel_last = now;
                    let dir = if dy > 0.0 { -1 } else { 1 }; // 上滚=上一个
                    self.play_adjacent(frame, dir);
                }
            }
        }

        // 每帧同步子窗口位置（含 ppp 变化/窗口缩放）
        let ppp = ctx.pixels_per_point();
        self.sync_child_rect(child, ppp);
    }

    fn controls(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
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
                    let m = ui.selectable_label(
                        false,
                        egui::RichText::new("静音").color(egui::Color32::RED).strong(),
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
            self.play_adjacent(frame, act.step);
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
    fn seek_bar(&mut self, ui: &mut egui::Ui) {
        let (dur, pos) = match &self.player {
            Some(p) => (p.duration().unwrap_or(0.0), p.time_pos().unwrap_or(0.0)),
            None => (0.0, 0.0),
        };
        let cur = if self.dragging { self.drag_pos } else { pos };

        let bar_rect = egui::Rect::from_min_size(
            ui.max_rect().min,
            egui::vec2(ui.available_width(), 18.0),
        );
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
            let ptr_val = |x: f32| {
                ((x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0) as f64 * dur
            };
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

/// 取 eframe 原生窗口 HWND（Windows，经 winit_window + raw-window-handle）
fn native_hwnd(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::HasWindowHandle;
    let w = frame.winit_window()?;
    let handle = w.window_handle().ok()?;
    if let raw_window_handle::RawWindowHandle::Win32(w) = handle.as_raw() {
        Some(w.hwnd.get() as isize)
    } else {
        None
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 播放中需要持续重绘刷新进度条
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        // 弹窗请求位重置：本帧各弹窗渲染时按需置位，帧末统一同步子窗口显隐
        self.overlay_open = false;
        // 拖放：压缩包文件拖入窗口即打开
        self.handle_dropped_files(&ctx);
        // 后台解压任务轮询（7z/RAR）
        self.poll_extract();
        // 键盘快捷键（mpv 风格）
        self.handle_shortcuts(&ctx, frame);
        // 图片自动翻页（PPT 式）
        self.slideshow_tick(&ctx, frame);
        // 密码输入弹窗（加密压缩包：加密头=打开阶段 / 加密条目=播放阶段）
        self.password_dialog(&ctx, frame);

        if self.fullscreen {
            // 全屏：隐藏所有面板，只留视频+底部进度条
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.video_area(ui, &ctx, frame));
            self.sync_child_visibility();
            return;
        }
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("controls").show(ui, |ui| self.controls(ui, frame));
        egui::Panel::left("files")
            .default_size(300.0)
            .show(ui, |ui| self.file_list(ui, frame));
        egui::CentralPanel::default().show(ui, |ui| self.video_area(ui, &ctx, frame));
        // 帧末统一同步：任一弹窗（帮助/密码框）打开期间隐藏 mpv 子窗口，全部关闭后恢复
        self.sync_child_visibility();
    }
}

impl App {
    /// 弹窗显隐 → mpv 子窗口同步（mpv 子窗口永远盖在 egui 绘制之上，
    /// 靠每帧重算的 overlay_open 请求位驱动，多弹窗嵌套开关时状态始终正确）
    fn sync_child_visibility(&mut self) {
        if self.overlay_open != self.child_hidden {
            self.child_hidden = self.overlay_open;
            if self.child_hwnd != 0 {
                use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
                unsafe {
                    ShowWindow(self.child_hwnd as _, if self.overlay_open { SW_HIDE } else { SW_SHOW });
                }
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.player = None; // 先销毁 mpv（含子窗口渲染）
        if self.child_hwnd != 0 {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.child_hwnd as _);
            }
            self.child_hwnd = 0;
        }
        // 退出时自动清空解压缓存（config: cache.clean_on_exit）
        if self.cfg.cache.clean_on_exit {
            let freed = cache::clean_all(&cache::cache_dir(&self.cfg));
            tracing::info!("退出清理缓存: {:.1} MB", freed as f64 / 1048576.0);
        }
    }
}
