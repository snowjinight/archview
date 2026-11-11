//! egui UI：左侧文件列表（标签页/搜索/状态行）+ 中央播放区（mpv render API
//! 纹理 blit，无原生子窗口）+ 底部控制栏。滚轮仅在预览窗口内悬停时切换
//! 媒体（ui.wheel_switch_secs 节流），文件列表区滚轮为正常滚动列表。
//!
//! 本文件只放共享骨架：状态类型、`App` 结构体、构造/字体安装、eframe 生命周期。
//! 功能按子模块拆分（均为 `App` 的拆分 `impl` 块，字段集中在此）：
//! - [`open`]     打开链路（对话框/拖放/后台打开线程/视图重建）
//! - [`extract`]  播放与后台解压链路（流式快路径、solid 滑窗会话、缓存命中）
//! - [`compare`]  多图对比视图（会话状态机 + 入口/键位编排，§16）
//! - [`dialogs`]  弹窗（密码输入、下一个压缩包选择）
//! - [`nav`]      快捷键与媒体导航（全屏、上一个/下一个、自动翻页）
//! - [`panels`]   UI 面板（顶栏/文件列表/媒体区/控制栏）与进度条绘制
//! - [`config_editor`] 内嵌 config.toml 编辑器（'#' 注释灰色高亮、保存即热生效）
//!
//! **目录 ≠ 层级**（层级 × 模块映射见 doc/架构设计/03_整体架构.md）：本目录
//! 是 GUI 应用线整体——panels/nav/dialogs/config_editor 是交互壳切片（L4），
//! open/extract/compare 是应用编排切片（L3）；媒体能力服务（player/imgview/
//! rawdec）与基建（vfs/search/config）在 crate 根，按能力命名。

use crate::cache;
use crate::config::Config;
use crate::imgview::{
    clamp_pan, fit_scale, zoom_pan, ImageService, ImageSource, ImgEvent, ImgKey, ImgView, MAX_ZOOM,
    MIN_ZOOM,
};
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

mod compare;
mod config_editor;
mod dialogs;
mod extract;
mod nav;
mod open;
mod panels;
#[cfg(test)]
mod tests;

// 对比视图共享符号（panels 绘制/入口谓词、nav 键位分派引用）；
// raw 软解服务住在 crate::rawdec 媒体层（与 ImageService 平级）
use crate::rawdec::RawFrameService;
use compare::{
    compare_eligible, grid_cells, pane_image_rect, clamp_center, sync_zoom_center, sync_zoom_step,
    CompareSession, PaneKind,
};

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

/// 「打开下一个压缩包」选择弹窗状态（本包末项后翻 / N 键主动触发共用）：
/// 同目录压缩包列表（分卷族已合并为首卷）+ 选中项下标（默认=循环意义上的下一个）
/// + at_end（true=末项后翻触发，文案提示“已到最后一项”；false=N 键主动触发）
struct NextArchivePrompt {
    archives: Vec<PathBuf>,
    sel: usize,
    at_end: bool,
}

/// 「编码为视频…」确认窗状态（§15.5 UI 执行）。打开时由选中集/右键行组装：
/// 多选（≥2）= 精确模式（所见即所选）；单选/无选 = 序列模式（窗内二选一，
/// 默认整族）。entries 为 entries 下标（自然序），fmt 来自命中预设（fps 可改）
struct EncodeDialog {
    /// 参与编码的条目下标（自然序；「仅所选」与「整族」切换时重建）
    entries: Vec<usize>,
    /// 单选二选一：另一档候选（整族）。多选精确模式 = None（无切换）
    alt_entries: Option<Vec<usize>>,
    /// 二选一当前档：true = entries（整族，默认）；false = alt_entries（仅此文件）
    whole: bool,
    /// 命中预设（显示族信息用；参数真身走 fmt）
    preset_match: String,
    /// 编码格式（fps 窗内可改，其余来自预设）
    fmt: crate::encode::EncodeFormat,
    /// 输出路径（默认 = 压缩包所在目录/目录本身 + 首帧基名.mp4；已存在拒绝）
    out: String,
    /// 编号断点等预警（警告不中止）
    warnings: Vec<String>,
}

/// 后台编码任务：线程跑 encode_raw_sequence，mpsc 回报进度/终态。
/// 同时仅一个（§15.5）；取消 = cancel 置位，引擎 stop 后返回 Err("已取消")
struct EncodeJob {
    cancel: Arc<AtomicBool>,
    rx: Receiver<EncodeMsg>,
    /// 输出路径（完成通知文案 + 已存在校验）
    out: PathBuf,
    started: std::time::Instant,
    /// 最新进度（0~100，poll_encode 更新，进度窗渲染）
    progress: f32,
}

/// 编码线程 → UI 消息（progress 回调在引擎线程，跨线程走 channel）
enum EncodeMsg {
    /// 0~100
    Progress(f32),
    Done(crate::encode::EncodeStats),
    Failed(String),
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
#[derive(Clone, Copy, PartialEq, Debug)]
enum ListTab {
    Video,
    Image,
}

/// 视频播放模式（三态，图片不适用——图片用 slideshow 自动翻页）。
/// L 键/控制栏按钮循环切换：Hold → List → Loop
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PlayMode {
    /// 定格（默认）：播完停在末帧不动（复用全局 keep-open=always 的
    /// EOF 末帧定格 + 粘滞暂停）
    #[default]
    Hold,
    /// 列表：播完自动切下一个（nav.rs video_eof_tick）
    List,
    /// 循环：单视频 mpv loop-file=inf 原生循环，EOF 不触发
    Loop,
}

/// 图片切换防闪保持画面：上一张最后一帧的绘制快照（纹理 + 屏幕矩形），
/// 下一张就绪前原样重画（绘制时按 video 区裁剪）。since 每次成功绘制
/// 刷新，因此 elapsed 度量的是"等待下一张"的时长而非观看时长
struct HoldImg {
    tex: egui::TextureHandle,
    rect: egui::Rect,
    since: std::time::Instant,
}

pub struct App {
    cfg: Config,
    archive: Option<ArchiveState>,
    search: String,
    /// 搜索防抖：输入变化置 dirty 记时间戳，停顿 cfg.ui.search_debounce_ms
    /// 后由 file_list 重建（0 = 本帧立即）；rebuild_view 统一清零
    search_dirty: bool,
    search_last_edit: Option<std::time::Instant>,
    /// 搜索历史缓存（最近优先）：搜索框聚焦时从磁盘刷新，记录/清空时同步
    /// 更新；UI 只取前 15 条展示。持久化见 search/history.rs
    search_history: Vec<String>,
    /// 当前播放的 entry（entries 下标）
    playing: Option<usize>,
    /// glow GL 上下文（启动期 CreationContext 注入，照搬开源参考方案）：
    /// PlayerService 惰性创建 mpv render context 时喂 get_proc_address；
    /// on_exit 时（GL 仍 current）销毁 render 资源。None = 非 glow 后端
    gl: Option<Arc<glow::Context>>,
    /// egui 上下文：mpv render update 回调跨线程 request_repaint 唤醒 UI 重绘
    egui_ctx: egui::Context,
    player: Option<PlayerService>,
    // 图片纹理直显服务：后台线程解码 → mpsc → UI 线程上传纹理（LRU 缓存）
    img: ImageService,
    // 解码失败已回退 mpv 的图片条目（archive_id, entry_idx）：tex_image_key 剔除
    // 避免"正在解码"占位遮挡 mpv 画面；play_image/load_extracted 直走 mpv 不重复解码
    img_fallback: std::collections::HashSet<ImgKey>,
    // 图片视图状态：zoom=None=适应窗口，Some(z)=100% 倍率；pan=平移偏移
    img_view: ImgView,
    // 上帧图片绘制区域（滚轮缩放锚点 / 拖拽限位用；未绘制过 = NOTHING）
    img_viewport: egui::Rect,
    // 落回"适应窗口"的闪现提示起点（右下角徽标显示约 1s；None=不在闪现期）
    img_fit_flash: Option<std::time::Instant>,
    // 换画日志去重：上次选画 (key, 是否 full)。同 key 下档位翻转才打一条
    // 换画日志；切图（新 key）静默重置，避免逐帧刷屏
    img_pick_full: Option<(ImgKey, bool)>,
    // 非 solid 图片切换防闪：下一张解码期间原样重画上一张（快照见 HoldImg），
    // 超过 2s 仍未就绪才退回占位提示（避免无限解压时用户毫无感知）
    hold_img: Option<HoldImg>,
    // 当前图片的解码源 (key, source, 文件名)：放大到原尺寸附近时用同一源
    // 按需解码不降采样的原图纹理；切视频/回退 mpv 时清空
    img_source: Option<(ImgKey, ImageSource, String)>,
    // 进度条拖动状态
    dragging: bool,
    drag_pos: f64,
    error: Option<String>,
    // 一次性提示（如缓存清理结果），显示在顶栏。与 error 互斥单槽：
    // 新消息覆盖旧消息，无任何超时（清除路径见 set_notice 注释）
    notice: Option<String>,
    // 编码完成绿条的伴随动作（§15.5）：Some 时绿条整条可点——点击
    // 打开输出所在目录。属旧消息的动作随新消息/播放/切内容源失效
    encode_done_dir: Option<std::path::PathBuf>,
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
    // 上一帧起始时刻（空闲输入帧节流用，见 ui() 的 MOTION_PACING）
    frame_start: Option<std::time::Instant>,
    // 帧率自检累计：(窗口起点, 帧数)，每 5s 输出一条 debug 日志
    fps_acc: (std::time::Instant, u32),
    // 视频播放模式（三态，图片不适用——图片用 slideshow）：默认定格
    //（播完停在末帧），L 键/控制栏按钮循环切换 Hold→List→Loop
    play_mode: PlayMode,
    // 列表播放 EOF 推进防重入：已处理过 EOF 的条目。推进无效果时（列表
    // 末尾停住/目标需先解压）防每帧重复触发；eof 复位时自动清空
    eof_handled: Option<usize>,
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
    // 本会话打开过的压缩包路径（切包弹窗中灰色弱化标记"看过"）
    seen_archives: std::collections::HashSet<PathBuf>,
    // 内嵌 config.toml 编辑器（「配置 → 编辑」打开；None = 关闭）
    config_editor: Option<config_editor::ConfigEditor>,
    // 启动参数传入的初始打开目标（SendTo「Archview player」/拖到 exe 上），首帧触发一次
    initial_open: Option<PathBuf>,
    // 单媒体文件入口（拖入/SendTo/命令行/「打开媒体文件」）的跳转目标文件名：
    // open_path 分流置位（媒体文件 → 父目录集合），打开完成时 apply_open_done
    // 消费——预切标签页 + 跳转播放该文件；每次 open_path 重置，防旧值残留
    pending_focus: Option<String>,
    // 文件列表多选（Ctrl+点击 toggle / Shift+范围 / Ctrl+A 全选，§15.5）：
    // 全局条目下标集（对 entries 稳定，跨搜索过滤保留），「编码为视频…」
    // 的对象。打开/切换压缩包时清空
    selected: std::collections::BTreeSet<usize>,
    // Shift 范围选锚点（最近一次 Ctrl 点击的行；无锚点 Shift 自行落地；
    // 无修饰点击=纯播放，不碰锚点——播放高亮自动追踪但锚点保持稳定）
    select_anchor: Option<usize>,
    // 「编码为视频…」确认窗（§15.5）：None = 关闭
    encode_dialog: Option<EncodeDialog>,
    // 后台编码任务（§15.5：同时仅一个；编码 mpv 实例独立，播放不受影响）
    encode_job: Option<EncodeJob>,
    // 多图对比会话（compare.rs）：Some = 对比模态（mpv 不在场，独立键表）
    compare: Option<CompareSession>,
    // raw 帧软解服务（对比栏专用：后台线程解码 → mpsc → UI 上传纹理）
    rawdec: RawFrameService,
}

impl App {
    pub fn new(cfg: Config, cc: &eframe::CreationContext, initial: Option<PathBuf>) -> Self {
        Self::install_cjk_font(&cfg, &cc.egui_ctx);
        // 非 UTF-8 字节兜底代码页（config: codepage，默认 gbk）：
        // zip 老包文件名解码 + 密码候选
        crate::vfs::set_codepage(&cfg.codepage);
        // 清理上次中断解压留下的 .part 半截文件
        cache::cleanup_partial(&cache::cache_dir(&cfg));
        // 图片高清升级配置回显：改 config 调阈值/锐化后，启动即可确认生效
        tracing::info!(
            "图片高清升级: 阈值×{:.2} 锐化 {}",
            cfg.ui.image_full_threshold,
            if cfg.ui.image_full_sharpen {
                "开"
            } else {
                "关"
            }
        );
        let img_cap = cfg.ui.image_cache_count.max(1);
        Self {
            cfg,
            archive: None,
            search: String::new(),
            search_dirty: false,
            search_last_edit: None,
            search_history: Vec::new(),
            playing: None,
            gl: cc.gl.clone(),
            egui_ctx: cc.egui_ctx.clone(),
            player: None,
            img: ImageService::new(cc.egui_ctx.clone(), img_cap),
            img_fallback: std::collections::HashSet::new(),
            img_view: ImgView::default(),
            img_viewport: egui::Rect::NOTHING,
            img_fit_flash: None,
            img_pick_full: None,
            hold_img: None,
            img_source: None,
            dragging: false,
            drag_pos: 0.0,
            error: None,
            notice: None,
            encode_done_dir: None,
            fullscreen: false,
            list_tab: ListTab::Video,
            scrolled_to: None,
            wheel_last: f64::NEG_INFINITY,
            slideshow_on: false,
            slideshow_idx: None,
            slideshow_anchor: 0.0,
            frame_start: None,
            fps_acc: (std::time::Instant::now(), 0),
            play_mode: PlayMode::Hold,
            eof_handled: None,
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
            config_editor: None,
            initial_open: initial,
            pending_focus: None,
            selected: Default::default(),
            select_anchor: None,
            encode_dialog: None,
            encode_job: None,
            compare: None,
            rawdec: RawFrameService::new(cc.egui_ctx.clone()),
        }
    }

    /// 测试专用构造：绕开 eframe::CreationContext（单测无 egui 上下文，
    /// CJK 字体安装跳过），其余初始化与 App::new 一致
    #[cfg(test)]
    fn new_for_test(cfg: Config) -> Self {
        cache::cleanup_partial(&cache::cache_dir(&cfg));
        let ctx = egui::Context::default();
        let img_cap = cfg.ui.image_cache_count.max(1);
        Self {
            cfg,
            archive: None,
            search: String::new(),
            search_dirty: false,
            search_last_edit: None,
            search_history: Vec::new(),
            playing: None,
            gl: None,
            egui_ctx: ctx.clone(),
            player: None,
            img: ImageService::new(ctx.clone(), img_cap),
            img_fallback: std::collections::HashSet::new(),
            img_view: ImgView::default(),
            img_viewport: egui::Rect::NOTHING,
            img_fit_flash: None,
            img_pick_full: None,
            hold_img: None,
            img_source: None,
            dragging: false,
            drag_pos: 0.0,
            error: None,
            notice: None,
            encode_done_dir: None,
            fullscreen: false,
            list_tab: ListTab::Video,
            scrolled_to: None,
            wheel_last: f64::NEG_INFINITY,
            slideshow_on: false,
            slideshow_idx: None,
            slideshow_anchor: 0.0,
            frame_start: None,
            fps_acc: (std::time::Instant::now(), 0),
            play_mode: PlayMode::Hold,
            eof_handled: None,
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
            config_editor: None,
            initial_open: None,
            pending_focus: None,
            selected: Default::default(),
            select_anchor: None,
            encode_dialog: None,
            encode_job: None,
            compare: None,
            rawdec: RawFrameService::new(ctx.clone()),
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

    /// 红色错误：互斥覆盖（清掉绿色提示）。无超时，常驻至被新消息或
    /// 成功播放新内容覆盖（play/play_image/load_extracted/切换内容源）
    pub(super) fn set_err(&mut self, e: impl Into<String>) {
        let e = e.into();
        tracing::error!("{e}");
        self.notice = None;
        self.encode_done_dir = None;
        self.error = Some(e);
    }

    /// 绿色提示：互斥覆盖（清掉红色错误）。无超时——任意鼠标点击即清
    ///（update 帧首检测，用户操作了即视为已读），新消息覆盖旧消息
    pub(super) fn set_notice(&mut self, n: impl Into<String>) {
        self.error = None;
        // 新消息不继承旧消息的伴随动作（调用方随后可再挂自己的动作）
        self.encode_done_dir = None;
        self.notice = Some(n.into());
    }

    /// 帧首：任意鼠标点击即清绿色提示（用户操作了=看过了，无需逐按钮
    /// 判断）。先清后写——本帧稍后由此点击产生的新提示不受影响。
    /// 只清绿——红色错误代表操作失败，不被无关点击消除。
    /// 例外：编码完成绿条（encode_done_dir 在场）不可由此清——点击
    /// 正是它的"打开输出目录"手势（帧首先清会让控件回调永远收不到
    /// 这次点击），由 panels.rs 绿条自身消费并消条
    pub(super) fn clear_notice_on_click(&mut self, ctx: &egui::Context) {
        if self.notice.is_some()
            && self.encode_done_dir.is_none()
            && ctx.input(|i| i.pointer.any_click())
        {
            self.notice = None;
        }
    }
}

impl eframe::App for App {
    // #[allow]：帧探针 st 在全屏早退路径的末次赋值无人读
    #[allow(unused_assignments)]
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // —— 输入帧节流 ——
        // egui-winit 对 CursorMoved 无条件 request_repaint（egui-winit 0.36
        // lib.rs:352-358，无内置节流；eframe 的 max_fps 仅 web 后端实现），
        // 鼠标事件率（125~1000Hz）直接驱动帧率：空闲态每帧便宜，帧率可达
        // 数百 fps → 悬停 CPU/GPU 各 ~10%（随移动速度攀升）。
        // 处置：非视频播放态统一节流，按触发源分档（间隔由 [ui] 三个
        // redraw_*_fps 配置驱动，见 config.rs pacing_ms，钳制 5~240ms→4~200ms）：
        // - 拖拽中（指针按下）：redraw_drag_fps（默认 60fps），保拖拽/平移流畅；
        // - 悬停/滚轮（PointerMoved/MouseWheel 且未按下）：redraw_hover_fps
        //   （默认 30fps）——60fps 全量重建在弱 GPU（RX 640 实测 11~17%）/
        //   弱 CPU 上仍占 ~10%，30fps 无感知，占用减半；
        // - 其余帧（线程唤醒/timer/hover 动画）：redraw_other_fps（默认
        //   60fps）——解码完成等事件延迟 ≤ 帧间隔，解压进度唤醒风暴同样封顶。
        // 视频播放态排除：vsync 主钟已封顶，节流 no-op 且睡眠会推过 vsync
        // 线制造 judder。幻灯片/动图 timer 帧间隔为秒级/帧延迟，远大于悬停
        // 间隔上限（pacing 钳制最大 200ms），不触发睡眠，翻页精度不受影响。
        // frame_start 记录**补眠后**的实际帧起始：若记补眠前时刻，帧间距
        // 会漂移成"短+长"交替（平均 ~100fps 而非 60fps）
        let video_live = self.playing.is_some()
            && self.tex_image_key().is_none()
            && self.player.as_ref().is_some_and(|p| !p.paused());
        let dragging = ctx.input(|i| i.pointer.primary_down());
        // 悬停档触发源：指针移动 + 滚轮。egui-winit 对 MouseWheel 同样无条件
        // request_repaint，列表滚到底后滚动量被 ScrollArea 钳制、画面无变化，
        // 但 repaint 已在事件层排掉（应用层只能节流不能取消）——快速滚轮
        // 帧率同样可到 ~60fps 且全是 no-op 全量重建，归入悬停档 30fps
        let motion = ctx.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::PointerMoved(_) | egui::Event::MouseWheel { .. }))
        });
        let pacing_ms = |fps: u32| {
            std::time::Duration::from_millis(crate::config::UiConfig::pacing_ms(fps))
        };
        let pacing = if dragging {
            pacing_ms(self.cfg.ui.redraw_drag_fps)
        } else if motion {
            pacing_ms(self.cfg.ui.redraw_hover_fps)
        } else {
            pacing_ms(self.cfg.ui.redraw_other_fps)
        };
        let frame_t = std::time::Instant::now();
        if !video_live {
            if let Some(prev) = self.frame_start {
                let elapsed = frame_t.duration_since(prev);
                if elapsed < pacing {
                    std::thread::sleep(pacing - elapsed);
                }
            }
        }
        self.frame_start = Some(std::time::Instant::now());
        // —— 帧率自检（debug 级，每 5s 一条）：验证节流生效的观测手段，
        // RUST_LOG=archview=debug 可见；release 默认关闭零输出。
        // 合理区间：悬停 ~30fps，拖拽 ≤60fps，空闲 0
        self.fps_acc.1 += 1;
        if frame_t.duration_since(self.fps_acc.0) >= std::time::Duration::from_secs(5) {
            let fps = self.fps_acc.1 as f64 / frame_t.duration_since(self.fps_acc.0).as_secs_f64();
            tracing::debug!("[帧率自检] {fps:.0}fps");
            self.fps_acc = (frame_t, 0);
        }
        // —— 帧探针（常驻条件诊断，debug 级）——
        // 分段 ≥30ms 打点、帧总 ≥50ms 两个出口打点，平时零输出。
        // segments 覆盖每帧属性读：eof_tick=eof_reached、top/controls=
        // time_pos/duration/buffering
        let t0 = std::time::Instant::now();
        let mut st = t0;
        macro_rules! probe {
            ($name:expr) => {{
                let now = std::time::Instant::now();
                let d = now.duration_since(st).as_secs_f64() * 1000.0;
                if d >= 30.0 {
                    tracing::debug!("[帧探针] {} {d:.1}ms", $name);
                }
                st = now;
            }};
        }
        // 视频播放中：逐 vsync 连续重绘，让 eframe 的 swap 成为节拍主钟。
        // render() 已设 BLOCK_FOR_TARGET_TIME=0 不再阻塞到目标时刻，每次渲染
        // "当前应显示"的帧，换帧时刻锁定 vsync 边界，消除"mpv 回调唤醒 →
        // 事件循环调度"链路上 1~15ms 抖动导致的帧节奏不均（可感知卡顿）。
        // 暂停/EOF 粘滞/装载期不排帧：EOF 与错误由 mpv 泵线程 send 后主动
        // 唤醒（player/mod.rs），装载期首帧轮询由 panels.rs 装载窗 16ms
        // 条件排帧覆盖。图片/对比/空闲态完全不排帧——解码（imgview/rawdec）、
        // 打开/解压/编码线程完成时各自 request_repaint 唤醒，输入事件由
        // egui 自动出帧；幻灯片开启时精确排到下次翻页（slideshow_rest）。
        // 全事件驱动：空闲/暂停/对比静置 CPU 趋近 0（无 10 FPS 全量重绘）；
        // 空闲态鼠标移动帧由帧首 MOTION_PACING 节流封顶 ~60Hz
        if video_live {
            ctx.request_repaint();
        } else if self.slideshow_on && self.tex_image_key().is_some() {
            let now = ctx.input(|i| i.time);
            ctx.request_repaint_after(self.slideshow_rest(now));
        }
        probe!("repaint决策");
        // 任意鼠标点击即清绿色提示（见 clear_notice_on_click；键盘操作
        // 不触发——键盘动作自身走 set_notice 覆盖或 play 清槽，语义自洽）
        self.clear_notice_on_click(&ctx);
        // 启动参数打开（SendTo「Archview player」/拖到 exe 上）：首帧触发一次
        if let Some(path) = self.initial_open.take() {
            self.open_path(path);
        }
        probe!("initial_open");
        // 拖放：压缩包文件拖入窗口即打开
        self.handle_dropped_files(&ctx);
        probe!("dropped");
        // 后台打开任务轮询（大包/网络盘）
        self.poll_open();
        probe!("poll_open");
        // 后台解压任务轮询（7z/RAR）
        self.poll_extract();
        probe!("poll_extract");
        // 图片解码结果轮询（后台线程 → UI 线程上传纹理）
        self.poll_img();
        probe!("poll_img");
        // 对比模式 tick（raw 帧轮询/对齐、image 栏自愈、非流式落地检查）
        self.compare_tick();
        probe!("compare_tick");
        // mpv 装载失败回显（END_FILE(ERROR) 通道 → set_err）
        self.poll_mpv_err();
        probe!("poll_mpv_err");
        // 键盘快捷键（mpv 风格）
        self.handle_shortcuts(&ctx, frame);
        probe!("shortcuts");
        // 图片自动翻页（PPT 式）
        self.slideshow_tick(&ctx);
        probe!("slideshow");
        // 列表播放：视频 EOF 自动切下一个（循环模式/图片不动作）
        self.video_eof_tick();
        probe!("eof_tick");
        // 密码输入弹窗（加密压缩包：加密头=打开阶段 / 加密条目=播放阶段）
        self.password_dialog(&ctx);
        probe!("pw_dialog");
        // 末项后询问打开同目录下一个压缩包
        self.next_archive_dialog(&ctx);
        probe!("next_archive");
        // 内嵌 config.toml 编辑器窗口（「配置 → 编辑」）
        self.config_editor_window(&ctx);
        probe!("config_editor");
        // 「编码为视频…」确认窗 + 后台编码任务轮询（§15.5 UI 执行）
        self.encode_dialog_window(&ctx);
        probe!("encode_dialog");
        self.poll_encode();
        probe!("poll_encode");

        if self.fullscreen {
            // 全屏：隐藏所有面板，只留视频+底部进度条
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.video_area(ui, &ctx));
            probe!("video_area");
            let tot = t0.elapsed().as_secs_f64() * 1000.0;
            if tot >= 50.0 {
                tracing::debug!("[帧探针] 全屏帧总 {tot:.1}ms");
            }
            return;
        }
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        probe!("top");
        // controls 面板 frame：底内边距在默认(2)上 +10，行底到窗口底恒为
        // 12px（bottom 面板锚定窗口底缘，行带是面板末元素，此间距不随内容
        // 高度变化；原行尾 add_space 会实推 7px 且位置随行内分配漂移）；
        // ♪ 的垂直居中由音量组 layout 交叉轴 Center 保证（见 panels.rs 注释）。
        let mut frame = egui::Frame::side_top_panel(ui.style());
        frame.inner_margin.bottom += 10;
        egui::Panel::bottom("controls")
            .frame(frame)
            .show(ui, |ui| self.controls(ui));
        probe!("controls");
        // 文件列表放左侧（视频区在右）；悬停行显示完整包内路径气泡
        //（路径/文件名分色）。render API 单平面后气泡不再被 mpv 原生
        // 子窗口遮挡，hover 已恢复（wid 时代曾因此禁用）
        // 默认宽度 220：列表只显示文件名（目录段在悬停气泡），不需要 300 宽
        egui::Panel::left("files")
            .default_size(220.0)
            .show(ui, |ui| self.file_list(ui));
        probe!("files");
        egui::CentralPanel::default().show(ui, |ui| self.video_area(ui, &ctx));
        probe!("video_area");
        let tot = t0.elapsed().as_secs_f64() * 1000.0;
        if tot >= 50.0 {
            tracing::debug!("[帧探针] 帧总 {tot:.1}ms");
        }
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
