//! egui UI：左侧文件列表（标签页/搜索/状态行）+ 中央播放区（mpv render API
//! 纹理 blit，无原生子窗口）+ 底部控制栏。滚轮仅在视频区悬停时切换媒体
//! （ui.wheel_switch_secs 节流），文件列表区滚轮为正常滚动列表。
//!
//! 本文件只放共享骨架：状态类型、`App` 结构体、构造/字体安装、eframe 生命周期。
//! 功能按子模块拆分（均为 `App` 的拆分 `impl` 块，字段集中在此）：
//! - [`open`]     打开链路（对话框/拖放/后台打开线程/视图重建）
//! - [`extract`]  播放与后台解压链路（流式快路径、solid 滑窗会话、缓存命中）
//! - [`dialogs`]  弹窗（密码输入、下一个压缩包选择）
//! - [`nav`]      快捷键与媒体导航（全屏、上一个/下一个、自动翻页）
//! - [`panels`]   UI 面板（顶栏/文件列表/媒体区/控制栏）与进度条绘制

use crate::cache;
use crate::config::Config;
use crate::player::mpv_ffi;
use crate::player::stream_cb;
use crate::player::PlayerService;
use crate::vfs::dir_vfs::DirVFS;
use crate::vfs::{
    archives_in_dir, next_archive_in_dir, open_with_password_probe, AccessMode, ArchiveVFS,
    SessionState, SharedSession, VfsError, VirtualEntry,
};
use egui::Widget;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

mod dialogs;
mod extract;
mod nav;
mod open;
mod panels;
#[cfg(test)]
mod tests;

/// 搜索框固定 Id：快捷键处理时仅对该控件让出键盘
fn search_box_id() -> egui::Id {
    egui::Id::new("search_box")
}

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
    /// 固实解码会话（UI ↔ 后台解码线程共享，生命周期与包一致；stream 包
    /// 不使用）。UI 组批时重写 targets/focus，解码线程动态消费——跳转只
    /// 更新目标集，不重启解码流（协议见 vfs::ArchiveVFS::extract_session）
    session: SharedSession,
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

/// 末项后询问打开下一个压缩包的弹窗状态：
/// 同目录压缩包列表（分卷族已合并为首卷）+ 选中项下标（默认=循环意义上的下一个）
struct NextArchivePrompt {
    archives: Vec<PathBuf>,
    sel: usize,
}

/// 后台解压任务（7z/RAR）：UI 侧的期望状态 + 消息通道。真正的解码循环
/// 在会话线程里（ArchiveState.session 伴生，见 vfs::ArchiveVFS::extract_session），
/// 跳转只重写会话目标集、不重启解码流；此对象描述"UI 现在想要什么"
struct ExtractJob {
    gen: u64,
    /// 当前 UI 期望播放的条目（批内导航时切换，不重启任务）
    entry_idx: usize,
    dest: PathBuf,
    entry_name: String,
    total: u64,
    done: u64,
    /// 当前阶段文字（空 = 正在写目标文件；非空 = solid 包跳过前置文件阶段，
    /// 此时 done = 已顺序解压的字节数）
    stage: String,
    /// 批内尚未解出的条目 → 目标路径（目标 + 预取）。条目落地（FileDone）
    /// 时移除；用户在批内切换目标时查此表决定"不取消只改期望"
    pending: std::collections::HashMap<usize, PathBuf>,
    /// 批内条目总数（含目标），显示"顺带解压 N 个"用
    batch_len: usize,
    /// 任务创建时刻：正在播放视频时切换到需解压的条目，超过宽限期
    /// （ui.extract_overlay_delay_ms）仍未解完才隐藏视频窗口显示进度——
    /// 快速解压全程不打扰播放，避免窗口闪烁。批内切换目标不重置
    /// （解压流本就在跑，切过去通常已在目标附近，立即显示进度是对的）
    started: std::time::Instant,
    /// 取消标志：仅切换压缩包时置位（终止解码线程，会话随旧包一起丢弃）。
    /// 批外跳转不取消——走会话重定向（session.targets 重写），解码流续跑
    /// 免重启（solid 包重启 = 重新顺序解前置数据）
    cancel: Arc<AtomicBool>,
    rx: Receiver<ExtMsg>,
}

enum ExtMsg {
    /// (字节数, 阶段文字)
    Progress(u64, String),
    /// 批内某条目解压落地（临时文件改名完成）：参数 = 条目下标
    FileDone(usize),
    Done,
    /// (错误信息, 是否密码问题)：密码问题弹密码框重试，其它报错
    Failed(String, bool),
}

/// 后台打开任务：大包（网络盘/万级条目）打开耗时可达分钟级，
/// 移出 UI 线程避免整个窗口冻结；期间保留旧包内容，界面照常可操作
struct OpenJob {
    gen: u64,
    path: PathBuf,
    /// 打开成功后要恢复播放的条目（replay_with_password 场景）
    retry_play: Option<(usize, VirtualEntry)>,
    rx: Receiver<OpenMsg>,
}

enum OpenMsg {
    /// 打开 + 注册完成（stream_cb 已注册，entries 已列出）。
    /// pw = 探测锁定的密码（None = 包未加密/无需密码；可能是 config 列表
    /// 里的密码而非提交密码——用于会话记忆与 ArchiveState.password）
    Done {
        vfs: Arc<dyn ArchiveVFS>,
        id: u64,
        entries: Vec<VirtualEntry>,
        pw: Option<String>,
    },
    /// 加密头或密码错误：需要（重新）弹密码框
    NeedPw {
        wrong: bool,
    },
    /// 进度更新（不结束任务）：fraction=None 表示不定进度阶段
    /// （如 zip crate 内部解析 CD，无法细分），Some(p) 为 0.0~1.0 真实百分比
    Progress {
        fraction: Option<f32>,
        stage: String,
    },
    Failed(String),
}

/// 文件列表的一行（虚拟化渲染的缓存行，见 App::list_rows）
struct ListRow {
    /// entries 下标
    idx: usize,
    /// 预格式化的显示文本 "[当前/总数] 文件名"
    label: String,
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
    /// glow GL 上下文（启动期 CreationContext 注入，照搬开源参考方案）：
    /// PlayerService 惰性创建 mpv render context 时喂 get_proc_address；
    /// on_exit 时（GL 仍 current）销毁 render 资源。None = 非 glow 后端
    gl: Option<Arc<glow::Context>>,
    /// egui 上下文：mpv render update 回调跨线程 request_repaint 唤醒 UI 重绘
    egui_ctx: egui::Context,
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
    // 后台打开（大包/网络盘：打开+列表移出 UI 线程）
    open_job: Option<OpenJob>,
    // 后台打开的当前进度：(fraction（None=不定进度）, 阶段文字)；任务结束即清
    open_progress: Option<(Option<f32>, String)>,
    next_gen: u64,
    // 最后一次实际解压的锚点（重定向时与会话 focus 同步写入；会话退出后
    // 保留）：「回到上次位置」的目标。不读会话 focus——会话死后点任何文件会
    // 开新会话使 focus 跳变，而这里始终指向用户上次真实观看推进的位置
    last_focus: Option<usize>,
    // 解压完成待消费：锚点落地时用户正在回看（keep 改写了期望目标，自动
    // 播放被吞）→ 置 true，顶栏显示"解压完成，点击观看"。播放到锚点即清
    extract_ready: bool,
    // 文件列表虚拟化：当前标签页的行缓存（打开/搜索/切标签时重建，不再每帧重算）
    list_rows: Vec<ListRow>,
    // (视频, 图片) 计数（view 内，随 list_rows 一起重建）
    list_counts: (usize, usize),
    // 列表行高实测缓存：show_rows 需要精确行高做滚动换算，
    // 首帧以 interact_size 估计，之后按实际渲染行高自校正
    row_h: Option<f32>,
    // 密码输入弹窗（加密压缩包）
    pw_prompt: Option<PwPrompt>,
    // 会话内密码记忆：压缩包路径 → 密码（内存中，退出即丢；切包回来不用重输）
    pw_memory: std::collections::HashMap<PathBuf, String>,
    // 末项后询问打开下一个压缩包：弹窗状态（目录内可选包列表 + 选中项）
    next_archive_prompt: Option<NextArchivePrompt>,
    // 本会话打开过的压缩包路径（切包弹窗中灰色标记"看过"）
    seen_archives: std::collections::HashSet<PathBuf>,
    // 启动参数传入的初始打开目标（SendTo「Archview player」/拖到 exe 上），首帧触发一次
    initial_open: Option<PathBuf>,
}

impl App {
    pub fn new(cfg: Config, cc: &eframe::CreationContext, initial: Option<PathBuf>) -> Self {
        Self::install_cjk_font(&cfg, &cc.egui_ctx);
        // 非 UTF-8 字节兜底代码页（config: codepage，默认 gbk）：
        // zip 老包文件名解码 + 密码候选
        crate::vfs::set_codepage(&cfg.codepage);
        // 清理上次中断解压留下的 .part 半截文件
        cache::cleanup_partial(&cache::cache_dir(&cfg));
        Self {
            cfg,
            archive: None,
            search: String::new(),
            playing: None,
            gl: cc.gl.clone(),
            egui_ctx: cc.egui_ctx.clone(),
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
            open_job: None,
            open_progress: None,
            next_gen: 0,
            last_focus: None,
            extract_ready: false,
            list_rows: Vec::new(),
            list_counts: (0, 0),
            row_h: None,
            pw_prompt: None,
            pw_memory: std::collections::HashMap::new(),
            next_archive_prompt: None,
            seen_archives: std::collections::HashSet::new(),
            initial_open: initial,
        }
    }

    /// 测试专用构造：绕开 eframe::CreationContext（单测无 egui 上下文，
    /// CJK 字体安装跳过），其余初始化与 App::new 一致
    #[cfg(test)]
    fn new_for_test(cfg: Config) -> Self {
        cache::cleanup_partial(&cache::cache_dir(&cfg));
        Self {
            cfg,
            archive: None,
            search: String::new(),
            playing: None,
            gl: None,
            egui_ctx: egui::Context::default(),
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
            open_job: None,
            open_progress: None,
            next_gen: 0,
            last_focus: None,
            extract_ready: false,
            list_rows: Vec::new(),
            list_counts: (0, 0),
            row_h: None,
            pw_prompt: None,
            pw_memory: std::collections::HashMap::new(),
            next_archive_prompt: None,
            seen_archives: std::collections::HashSet::new(),
            initial_open: None,
        }
    }

    /// 把 CJK 字体链追加进 egui 字体族（中文/日文/韩文不再显示方块）。
    /// 同族内多个字体按序尝试字形：雅黑覆盖汉字，日文字体补假名，Malgun 补谚文。
    /// 字体路径来自 config.json（"auto" = 系统字体目录中/日/韩候选），找不到则回退默认字体。
    fn install_cjk_font(cfg: &Config, ctx: &egui::Context) {
        let paths = cfg.cjk_font_paths();
        if paths.is_empty() {
            tracing::warn!(
                "未找到 CJK 字体，非拉丁字符将显示为方块（可在 config.toml 配 ui.font_path）"
            );
            return;
        }
        let mut fonts = egui::FontDefinitions::default();
        let mut loaded = 0usize;
        for (n, font_path) in paths.iter().enumerate() {
            match std::fs::read(font_path) {
                Ok(bytes) => {
                    let id = format!("cjk{n}");
                    fonts.font_data.insert(
                        id.clone(),
                        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                    );
                    // 追加到默认字体之后：拉丁字符仍用内置字体，CJK 按链序补字形
                    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                        fonts.families.entry(family).or_default().push(id.clone());
                    }
                    loaded += 1;
                }
                Err(e) => tracing::warn!("读取字体 {} 失败: {e}", font_path.display()),
            }
        }
        if loaded == 0 {
            tracing::warn!("所有 CJK 字体候选均读取失败，非拉丁字符将显示为方块");
            return;
        }
        ctx.set_fonts(fonts);
        tracing::info!("已加载 {loaded} 个 CJK 字体（中/日/韩字形链）");
    }

    pub(super) fn set_err(&mut self, e: impl Into<String>) {
        let e = e.into();
        tracing::error!("{e}");
        self.error = Some(e);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 播放中需要持续重绘刷新进度条
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        // 启动参数打开（SendTo「Archview player」/拖到 exe 上）：首帧触发一次
        if let Some(path) = self.initial_open.take() {
            self.open_path(path);
        }
        // 拖放：压缩包文件拖入窗口即打开
        self.handle_dropped_files(&ctx);
        // 后台打开任务轮询（大包/网络盘）
        self.poll_open();
        // 后台解压任务轮询（7z/RAR）
        self.poll_extract();
        // 键盘快捷键（mpv 风格）
        self.handle_shortcuts(&ctx, frame);
        // 图片自动翻页（PPT 式）
        self.slideshow_tick(&ctx);
        // 密码输入弹窗（加密压缩包：加密头=打开阶段 / 加密条目=播放阶段）
        self.password_dialog(&ctx);
        // 末项后询问打开同目录下一个压缩包
        self.next_archive_dialog(&ctx);

        if self.fullscreen {
            // 全屏：隐藏所有面板，只留视频+底部进度条
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.video_area(ui, &ctx, frame));
            return;
        }
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("controls").show(ui, |ui| self.controls(ui));
        // 文件列表放左侧（视频区在右）；悬停行的完整文件名显示在
        // 面板底部状态行（方案 A）——媒体区空间留给画面
        egui::Panel::left("files")
            .default_size(300.0)
            .show(ui, |ui| self.file_list(ui));
        egui::CentralPanel::default().show(ui, |ui| self.video_area(ui, &ctx, frame));
    }

    fn on_exit(&mut self, gl: Option<&glow::Context>) {
        // GL 仍 current：按序销毁 mpv render 资源（shutdown_render）后再整体销毁 mpv
        if let (Some(p), Some(gl)) = (self.player.as_mut(), gl) {
            p.shutdown_render(gl);
        }
        self.player = None;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // GL 资源清理在 eframe::App::on_exit（GL 仍 current 时销毁 render 资源，
        // 且早于 painter drop）；这里只剩 mpv terminate 与磁盘缓存清理
        // 退出时自动清空解压缓存（config: cache.clean_on_exit）
        if self.cfg.cache.clean_on_exit {
            let freed = cache::clean_all(&cache::cache_dir(&self.cfg));
            tracing::info!("退出清理缓存: {:.1} MB", freed as f64 / 1048576.0);
        }
    }
}
