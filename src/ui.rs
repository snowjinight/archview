//! egui UI：左侧文件列表（标签页/搜索/状态行）+ 中央播放区（mpv 子窗口嵌入
//! + 底部全宽进度条）+ 底部控制栏。滚轮仅在视频区悬停时切换媒体
//! （ui.wheel_switch_secs 节流），文件列表区滚轮为正常滚动列表。

use crate::cache;
use crate::config::Config;
use crate::player::stream_cb;
use crate::player::PlayerService;
use crate::vfs::{
    archives_in_dir, next_archive_in_dir, open_with_password_probe, AccessMode, ArchiveVFS,
    VfsError, VirtualEntry,
};
use egui::Widget;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
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

/// 末项后询问打开下一个压缩包的弹窗状态：
/// 同目录压缩包列表（分卷族已合并为首卷）+ 选中项下标（默认=循环意义上的下一个）
struct NextArchivePrompt {
    archives: Vec<PathBuf>,
    sel: usize,
}

/// 后台解压任务（7z/RAR）：目标 + 预取的一批条目（一次顺序解压流解出）
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
    /// 取消标志：用户切换到批外条目时置位，解压线程尽快退出
    /// （不取消会与新任务并行抢网络盘带宽，两边都变慢）
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
    // 后台打开（大包/网络盘：打开+列表移出 UI 线程）
    open_job: Option<OpenJob>,
    // 后台打开的当前进度：(fraction（None=不定进度）, 阶段文字)；任务结束即清
    open_progress: Option<(Option<f32>, String)>,
    next_gen: u64,
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
            open_job: None,
            open_progress: None,
            next_gen: 0,
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

    /// 打开压缩包；Password 错误（加密头）时弹密码框，提交后带密码重开。
    /// 打开+列表在后台线程执行（大包可达分钟级，不能冻结 UI）
    fn open_path_pw(&mut self, path: std::path::PathBuf, password: Option<&str>) {
        self.spawn_open(path, password.map(str::to_string), None);
    }

    /// 起后台线程打开压缩包（含 stream_cb 注册/列表，均为耗时步骤）。
    /// gen 与解压任务共用 next_gen：快速连续切换时旧结果直接丢弃
    fn spawn_open(
        &mut self,
        path: PathBuf,
        pw: Option<String>,
        retry_play: Option<(usize, VirtualEntry)>,
    ) {
        self.next_gen += 1;
        let gen = self.next_gen;
        let (tx, rx) = mpsc::channel::<OpenMsg>();
        // 线程闭包拿副本，本地保留原值用于 OpenJob 状态
        let path_t = path.clone();
        // 密码探测链候选：显式密码（会话记忆/用户输入）优先，
        // config 公用密码列表（播放器与 VarTidy 共享）兜底，去重保序
        let mut candidates: Vec<String> = Vec::new();
        for c in pw
            .iter()
            .filter(|p| !p.is_empty())
            .chain(self.cfg.passwords.iter().filter(|p| !p.is_empty()))
        {
            if !candidates.contains(c) {
                candidates.push(c.clone());
            }
        }
        let spawned = std::thread::Builder::new()
            .name("open_archive".into())
            .spawn(move || {
                // 阶段 1（crate 内部解析头/CD，无法细分）：不定进度
                let _ = tx.send(OpenMsg::Progress {
                    fraction: None,
                    stage: "正在读取压缩包目录".into(),
                });
                // 打开阶段即探测并锁定密码（ARCHITECTURE.md 12.5）：
                // 加密头 open 即验；内容加密对最小加密条目逐候选校验。
                // 探测在后台线程完成，错密码不会替换已打开状态，直接重弹
                let msg = match open_with_password_probe(&path_t, &candidates) {
                    Ok((vfs, locked)) => {
                        let arc: Arc<dyn ArchiveVFS> = vfs.into();
                        // 阶段 2（list，zip 的 CD 顺序遍历有真实百分比）
                        let (id, entries) =
                            stream_cb::register_with_progress(arc.clone(), &path_t, &|f, s| {
                                let _ = tx.send(OpenMsg::Progress {
                                    fraction: f,
                                    stage: s.to_string(),
                                });
                            });
                        OpenMsg::Done {
                            vfs: arc,
                            id,
                            entries,
                            pw: locked,
                        }
                    }
                    Err(VfsError::Password) => OpenMsg::NeedPw {
                        wrong: pw.is_some(),
                    },
                    Err(e) => OpenMsg::Failed(format!("Failed to open {}: {e}", path_t.display())),
                };
                let _ = tx.send(msg);
            })
            .is_ok();
        if spawned {
            self.open_job = Some(OpenJob {
                gen,
                path,
                retry_play,
                rx,
            });
            self.open_progress = None;
        } else {
            // 线程都起不来：直接报错（极罕见）
            self.set_err(format!("无法启动打开线程: {}", path.display()));
        }
    }

    /// 每帧轮询后台打开任务；Progress 消息只更新进度状态，
    /// 终态（Done/NeedPw/Failed/线程中断）完成后套用与旧同步路径相同的状态迁移
    fn poll_open(&mut self, frame: &mut eframe::Frame) {
        loop {
            // 借用结束后再处理，避免 &self（job）与状态迁移的 &mut self 冲突
            let msg = match self.open_job.as_ref().map(|j| j.rx.try_recv()) {
                Some(Ok(m)) => m,
                Some(Err(mpsc::TryRecvError::Empty)) => return,
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    OpenMsg::Failed("打开线程中断".into())
                }
                None => return, // 任务已在终态处理中被 take
            };
            // 进度：仅更新显示状态，任务继续跑（matches! 不 move，msg 留给终态 match）
            if matches!(msg, OpenMsg::Progress { .. }) {
                if let OpenMsg::Progress { fraction, stage } = msg {
                    if self
                        .open_job
                        .as_ref()
                        .is_some_and(|j| j.gen == self.next_gen)
                    {
                        self.open_progress = Some((fraction, stage));
                    }
                }
                continue;
            }
            let job = self.open_job.take().unwrap();
            // 已被更新的打开/解压任务取代：丢弃旧结果
            if job.gen != self.next_gen {
                return;
            }
            self.open_progress = None; // 终态：清进度显示
            match msg {
                OpenMsg::Done {
                    vfs,
                    id,
                    entries,
                    pw,
                } => {
                    let path = job.path;
                    // 切换压缩包：按配置清理上一个包的解压缓存（默认不清理——保留可命中）
                    if self.cfg.cache.clean_on_switch {
                        if let Some(old) = &self.archive {
                            let freed = cache::clean_archive(&cache::cache_dir(&self.cfg), old.id);
                            if freed > 0 {
                                tracing::info!(
                                    "切换压缩包，清理旧缓存 {:.1} MB",
                                    freed as f64 / 1048576.0
                                );
                            }
                        }
                    }
                    let stream = vfs.access_mode() == AccessMode::FastSeek;
                    // 密码记忆：探测锁定密码的包记入会话表（切回来不用重探）
                    if let Some(pw) = &pw {
                        self.pw_memory.insert(path.clone(), pw.clone());
                    }
                    // 会话内"看过"标记（切包弹窗中灰色显示）
                    self.seen_archives.insert(path.clone());
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.archive = Some(ArchiveState {
                        id,
                        name,
                        path: path.clone(),
                        password: pw.clone(),
                        entries,
                        view: Vec::new(),
                        vfs,
                        stream,
                    });
                    self.rebuild_view();
                    tracing::info!("打开压缩包: {}（{} 项）", path.display(), self.archive.as_ref().map(|a| a.entries.len()).unwrap_or(0));
                    // 取消并清掉旧包的解压任务：不清会导致旧包 FileDone 到来时
                    // load_extracted 把画面切回旧包条目（playing 下标错乱）
                    if let Some(j) = &self.extract {
                        tracing::info!("取消解压任务: {}（切换压缩包）", j.entry_name);
                        j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.extract = None;
                    self.playing = None;
                    self.error = None;
                    self.pw_prompt = None;
                    // 密码重试场景：恢复播放提交密码时想看的条目
                    if let Some((entry_idx, _)) = job.retry_play {
                        self.play(entry_idx, frame);
                    }
                }
                OpenMsg::NeedPw { wrong } => {
                    self.pw_prompt = Some(PwPrompt {
                        kind: match job.retry_play {
                            Some((entry_idx, _)) => PwKind::Play(entry_idx),
                            None => PwKind::Open(job.path),
                        },
                        input: String::new(),
                        wrong,
                    });
                }
                OpenMsg::Failed(e) => self.set_err(e),
                OpenMsg::Progress { .. } => unreachable!("Progress 已在上方提前 continue"),
            }
            return; // 终态处理完毕，本轮轮询结束
        }
    }

    /// 重新计算过滤视图：搜索框子串匹配 + 只显示媒体文件（视频/图片按 config 分类），
    /// 按文件名自然序排序（数字段按数值比较：1/2/10，而非字典序的 1/10/2；
    /// 保证序号/上一个下一个顺序稳定，与归档内部顺序无关）。
    /// 同时重建列表行缓存（虚拟化渲染的显示数据）：只在打开/搜索/切标签时执行，
    /// 不再每帧重算——万级条目时每帧 clone/format 是 UI 卡顿的主因
    fn rebuild_view(&mut self) {
        let Some(a) = &mut self.archive else {
            self.list_rows.clear();
            self.list_counts = (0, 0);
            return;
        };
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
        // 排序（config: ui.sort_files，默认关）：自然序（alphanumeric-sort），
        // 数字段按数值比较（1/2/10），非数字段不区分大小写——看漫画/图集时开启。
        // 默认关闭 = 保持压缩包内部顺序：7z/RAR 固态包下内部顺序 == 物理码流顺序，
        // 预取批恰好是物理相邻的后续文件，翻页直接命中缓存（性能最优）。
        // 注意 7-Zip 工具打的包内部本来就是字典序（实验验证：无视添加顺序强制重排），
        // 关闭排序对这类包的显示无变化，属正常现象。
        if self.cfg.ui.sort_files {
            // 预生成小写键（每条一次分配），比较器内不再重复 to_lowercase
            // （旧比较器每次比较做 2 次 to_lowercase，万级条目 ≈ 34 万次分配）。
            // 用 (idx, key) 对排序再取回 idx：键与位置绑定，排序中不会错位
            let mut keyed: Vec<(usize, String)> = a
                .view
                .iter()
                .map(|&i| (i, a.entries[i].name.to_lowercase()))
                .collect();
            keyed.sort_by(|x, y| alphanumeric_sort::compare_str(&x.1, &y.1));
            a.view = keyed.into_iter().map(|(i, _)| i).collect();
        }

        // 自动切标签页：当前标签页为空而另一类非空时跳过去。
        // 纯图片/纯视频的包打开时不再停在空列表上让用户误以为没打开
        // （搜索过滤后的空结果不切——那属于"没匹配"而非"没这类文件"，
        //   需要搜索框为空才切，避免打断用户在另一标签页里搜东西）
        if search.is_empty() {
            let (n_video, n_image) = {
                let mut c = (0usize, 0usize);
                for &i in &a.view {
                    if self.cfg.is_video_ext(&a.entries[i].name) {
                        c.0 += 1;
                    } else {
                        c.1 += 1;
                    }
                }
                c
            };
            let (cur, other) = match self.list_tab {
                ListTab::Video => (n_video, n_image),
                ListTab::Image => (n_image, n_video),
            };
            if cur == 0 && other > 0 {
                self.list_tab = match self.list_tab {
                    ListTab::Video => ListTab::Image,
                    ListTab::Image => ListTab::Video,
                };
                tracing::debug!(
                    "当前标签页为空，自动切换到{}标签页",
                    match self.list_tab {
                        ListTab::Video => "视频",
                        ListTab::Image => "图片",
                    }
                );
            }
        }

        // 行缓存：当前标签页的行 + 两类计数（view 内）
        let mut counts = (0usize, 0usize);
        let mut tab_items: Vec<(usize, &str)> = Vec::new();
        for &i in &a.view {
            let e = &a.entries[i];
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
            tab_items.push((i, e.name.as_str()));
        }
        let total = tab_items.len();
        self.list_rows = tab_items
            .into_iter()
            .enumerate()
            .map(|(n, (idx, name))| ListRow {
                idx,
                // 换行符清洗成空格：内嵌 \n 会把行撑成多行高，破坏虚拟化等高前提
                label: format!("[{}/{}] {}", n + 1, total, name).replace(['\n', '\r'], " "),
            })
            .collect();
        self.list_counts = counts;
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
            let need_pw = self
                .archive
                .as_ref()
                .is_some_and(|a| a.entries[entry_idx].encrypted && a.password.is_none());
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

    /// 启动后台解压任务；缓存命中（同尺寸文件已在）则直接播放。
    /// 目标 + 后续预取条目组成一批，一次顺序解压流解出（solid 包定位目标
    /// 需顺序解压前置数据，一次解一批摊薄固定成本，翻页直接命中缓存）
    fn start_extract(&mut self, entry_idx: usize) {
        // 去重：该条目的解压线程已在跑（双击/重复点击会瞬时触发两次 play），
        // 不再起第二个线程——两个 unrar 抢写同一个临时文件会报 "Could not create file"。
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
                // 旧任务处理：目标已可直接播——
                // · 目标在旧批内：只切期望目标（批继续预取后续条目；
                //   FileDone 到来时 playing 已是该条目，不会重播）
                // · 不在批内：用户已跳走，取消旧任务（不取消会与新任务
                //   并行抢网络盘带宽，且旧线程无 journal 可见性——曾表现为
                //   "多条后台解压日志但看不到打断"）
                if self
                    .extract
                    .as_ref()
                    .is_some_and(|j| j.pending.contains_key(&entry_idx))
                {
                    let j = self.extract.as_mut().unwrap();
                    j.entry_idx = entry_idx;
                    j.dest = dest.clone();
                    j.entry_name = entry.name.clone();
                    j.total = entry.size;
                    j.done = 0;
                    j.stage = String::new();
                    tracing::info!("批内切换目标（缓存命中）: {}", entry.name);
                } else if let Some(j) = &self.extract {
                    tracing::info!("取消解压任务: {}（新目标已缓存命中）", j.entry_name);
                    j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.extract = None;
                }
                self.load_extracted(&dest, entry_idx);
                return;
            }
        }

        // 批内切换：新目标在当前批的未完成清单里 → 不取消不重启，
        // 只把期望目标切过去；该文件落地时（FileDone）立即播放
        if self
            .extract
            .as_ref()
            .is_some_and(|j| j.pending.contains_key(&entry_idx))
        {
            let j = self.extract.as_mut().unwrap();
            j.entry_idx = entry_idx;
            j.dest = dest;
            j.entry_name = entry.name.clone();
            j.total = entry.size;
            j.done = 0;
            j.stage = String::new();
            tracing::info!("批内切换目标: {}", entry.name);
            return;
        }

        // 组批：目标 + view 顺序（显示顺序）后续条目，累计不超过预取上限。
        // 已缓存的条目跳过（无需解压、不占批额度）
        let cap = self.cfg.extract.prefetch_mb.saturating_mul(1024 * 1024);
        let mut batch: Vec<usize> = vec![entry_idx];
        let mut batch_bytes = entry.size;
        if cap > 0 {
            if let Some(pos) = arch.view.iter().position(|&i| i == entry_idx) {
                for &i in arch.view[pos + 1..].iter() {
                    if batch_bytes >= cap {
                        break;
                    }
                    let e = &arch.entries[i];
                    if e.is_dir {
                        continue;
                    }
                    let d = cache::dest_path(&dir, arch.id, i, &e.name);
                    if std::fs::metadata(&d)
                        .map(|m| m.len() == e.size)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    batch_bytes += e.size;
                    batch.push(i);
                }
            }
        }

        // 磁盘余量预检（整批）：缓存目录所在盘放不下（留 64MB 余量）则直接报错，
        // 不发起到注定失败的解压。查不到余量（目录不可达等）跳过，让解压错误自己暴露。
        if let Some(free) = cache::free_bytes(&dir) {
            let need = batch_bytes.saturating_add(64 * 1024 * 1024);
            if free < need {
                let msg = format!(
                    "缓存目录所在磁盘空间不足：剩余 {:.1}GB，本批 {} 个条目共需 {:.1}GB（缓存目录 [{}]，可清理缓存或修改 config.toml 的 cache.dir / extract.prefetch_mb）",
                    free as f64 / (1024.0 * 1024.0 * 1024.0),
                    batch.len(),
                    need as f64 / (1024.0 * 1024.0 * 1024.0),
                    dir.display()
                );
                tracing::error!("{msg}");
                self.error = Some(msg);
                return;
            }
        }

        cache::evict_if_needed(&dir, self.cfg.cache.max_size_mb * 1024 * 1024, batch_bytes);

        // 取消旧解压任务：新目标不在旧批内（solid 包跳过前置文件可达分钟级，
        // 旧任务继续跑只会与新任务并行抢网络盘带宽）
        if let Some(j) = &self.extract {
            tracing::info!("取消解压任务: {}（新目标不在旧批内）", j.entry_name);
            j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        self.next_gen += 1;
        let gen = self.next_gen;
        let (tx, rx) = mpsc::channel::<ExtMsg>();
        let name = entry.name.clone();
        let total = entry.size;
        let batch_len = batch.len();
        let batch_entries: Vec<crate::vfs::VirtualEntry> =
            batch.iter().map(|&i| arch.entries[i].clone()).collect();
        let batch_dests: Vec<PathBuf> = batch
            .iter()
            .map(|&i| {
                let e = &arch.entries[i];
                cache::dest_path(&dir, arch.id, i, &e.name)
            })
            .collect();
        let pending: std::collections::HashMap<usize, PathBuf> = batch
            .iter()
            .copied()
            .zip(batch_dests.iter().cloned())
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = cancel.clone();
        std::thread::Builder::new()
            .name("extract".into())
            .spawn(move || {
                let trefs: Vec<(&crate::vfs::VirtualEntry, &Path)> = batch_entries
                    .iter()
                    .zip(batch_dests.iter())
                    .map(|(e, d)| (e, d.as_path()))
                    .collect();
                let idxs = batch.clone();
                let r = vfs.extract_batch(
                    &trefs,
                    &cancel_for_thread,
                    &|done, stage| {
                        let _ = tx.send(ExtMsg::Progress(done, stage.to_string()));
                    },
                    &|ti| {
                        let _ = tx.send(ExtMsg::FileDone(idxs[ti]));
                    },
                );
                match r {
                    Ok(()) => {
                        let _ = tx.send(ExtMsg::Done);
                    }
                    Err(_) if cancel_for_thread.load(std::sync::atomic::Ordering::Relaxed) => {
                        // 任务已被新目标取代（cancel 置位）：结果无意义，静默退出
                    }
                    Err(e) => {
                        let is_pwd = matches!(e, VfsError::Password);
                        let _ = tx.send(ExtMsg::Failed(e.to_string(), is_pwd));
                    }
                }
            })
            .ok();

        tracing::info!(
            "后台解压: {name} → {}（顺带预取 {} 项）",
            dest.display(),
            batch_len - 1
        );
        self.extract = Some(ExtractJob {
            gen,
            entry_idx,
            dest,
            entry_name: name,
            total,
            done: 0,
            stage: String::new(),
            pending,
            batch_len,
            started: std::time::Instant::now(),
            cancel,
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
                    ExtMsg::Failed(format!("解压线程中断（条目 [{}]）", job.entry_name), false)
                }
            };
            let (gen, entry_idx, dest, entry_name) = (
                job.gen,
                job.entry_idx,
                job.dest.clone(),
                job.entry_name.clone(),
            );
            match msg {
                ExtMsg::Progress(done, stage) => {
                    if let Some(j) = &mut self.extract {
                        j.done = done;
                        j.stage = stage;
                    }
                    return; // 本帧处理完毕，下帧继续
                }
                ExtMsg::FileDone(i) => {
                    // 批内条目落地：是当前期望目标则立即播放（预取的留在缓存里
                    // 供后续翻页命中），否则只记账
                    let mut play: Option<(PathBuf, usize)> = None;
                    if let Some(j) = &mut self.extract {
                        if let Some(d) = j.pending.remove(&i) {
                            // 缓存命中路径已直接播放该条目时不重播
                            // （重播会让视频闪断、图片重置动画）
                            if i == j.entry_idx && self.playing != Some(i) {
                                play = Some((d, i));
                            }
                        }
                    }
                    if let Some((d, i)) = play {
                        self.load_extracted(&d, i);
                    }
                }
                ExtMsg::Done => {
                    self.extract = None;
                    // 兜底：目标理论上已由 FileDone 播放（gen 匹配 = 任务未被取代
                    // 且未被批内切走）；文件在而没播过才补播
                    if gen == self.next_gen && self.playing != Some(entry_idx) {
                        if std::fs::metadata(&dest).is_ok() {
                            self.load_extracted(&dest, entry_idx);
                        }
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

    /// 密码提交后：带密码重开压缩包（重建 VFS）并重试播放指定 entry。
    /// 重开同样走后台线程（zip 密码校验在 open_job 线程内做，错密码不替换状态、直接重弹）
    fn replay_with_password(&mut self, entry_idx: usize, pw: String) {
        let Some(arch) = self.archive.as_ref() else {
            return;
        };
        let path = arch.path.clone();
        let entry = arch.entries[entry_idx].clone();
        self.spawn_open(path, Some(pw), Some((entry_idx, entry)));
    }

    /// 密码输入弹窗（加密头=打开阶段 / 加密条目=播放阶段）。
    /// 弹窗期间隐藏 mpv 子窗口，窗口视口居中
    fn password_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = self.pw_prompt.as_mut() else {
            return;
        };
        // 密码框显示期间隐藏 mpv 子窗口（与帮助弹窗同机制，帧末统一同步）；
        // anchor 视口居中：egui 逻辑坐标下 125%/150% 缩放均保持居中
        self.overlay_open = true;
        let mut submitted = false;
        let mut cancelled = false;
        let title = match &p.kind {
            PwKind::Open(_) => "压缩包已加密",
            PwKind::Play(_) => "文件已加密",
        };
        egui::Window::new(title)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
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
                ui.add_space(4.0);
                // 按钮行右对齐（主按钮在最右）。先包一层 horizontal 限定单行高度，
                // 否则 right_to_left 会占满剩余区域并把按钮垂直居中"悬浮"在框中部
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("确定").clicked() {
                            submitted = true;
                        }
                        if ui.button("取消").clicked() {
                            cancelled = true;
                        }
                    });
                });
            });
        if submitted {
            let prompt = self.pw_prompt.take().unwrap();
            match prompt.kind {
                PwKind::Open(path) => self.open_path_pw(path, Some(&prompt.input)),
                PwKind::Play(idx) => self.replay_with_password(idx, prompt.input),
            }
        } else if cancelled {
            self.pw_prompt = None;
        }
    }

    /// 末项后询问打开下一个压缩包的确认弹窗（与密码框同机制：
    /// overlay_open 置位隐藏 mpv 子窗口，帧末统一同步显隐）。
    /// 列出同目录全部压缩包（分卷族合并为首卷），默认选中循环意义上的
    /// 下一个（末包绕回第一个），点选任意包可直接打开；
    /// 本会话打开过的包灰色显示，帮助绕圈时分辨看过的。
    fn next_archive_dialog(&mut self, ctx: &egui::Context) {
        // 先取当前包路径（"当前"标记）与已看集合（灰色标记），避免与 prompt 可变借用冲突
        let cur_name = self
            .archive
            .as_ref()
            .and_then(|a| a.path.file_name())
            .map(|n| n.to_string_lossy().to_lowercase());
        let seen = self.seen_archives.clone();
        let Some(prompt) = self.next_archive_prompt.as_mut() else {
            return;
        };
        self.overlay_open = true;
        let mut open = false;
        let mut cancelled = false;
        egui::Window::new("打开下一个压缩包？")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.label("已到本压缩包的最后一项，选择要打开的压缩包：");
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (i, path) in prompt.archives.iter().enumerate() {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let is_cur = cur_name
                                .as_deref()
                                .is_some_and(|c| name.to_lowercase() == c);
                            let label = if is_cur {
                                format!("{name}（当前）")
                            } else {
                                name
                            };
                            // 本会话看过的包灰色弱化，未看的正常色
                            let text = if seen.contains(path) {
                                egui::RichText::new(label).weak()
                            } else {
                                egui::RichText::new(label)
                            };
                            if ui.selectable_label(prompt.sel == i, text).clicked() {
                                prompt.sel = i;
                            }
                        }
                    });
                ui.add_space(4.0);
                // 按钮行右对齐（主按钮在最右）。先包一层 horizontal 限定单行高度，
                // 否则 right_to_left 会占满剩余区域并把按钮垂直居中"悬浮"在框中部
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("打开").clicked() {
                            open = true;
                        }
                        if ui.button("取消").clicked() {
                            cancelled = true;
                        }
                    });
                });
            });
        if open {
            let prompt = self.next_archive_prompt.take().unwrap();
            let path = prompt.archives[prompt.sel].clone();
            // 复用完整打开链路：pw_memory 密码记忆 / clean_on_switch 缓存清理 / 加密头弹密码框
            self.open_path(path);
        } else if cancelled {
            self.next_archive_prompt = None;
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
            self.play_adjacent(frame, step_entry, step_entry > 0);
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
    /// ask_next = 用户显式的"下一个"操作（快捷键/滚轮/按钮）：已在本包最后一项
    /// 再往后翻时，询问是否打开同目录下一个压缩包（config: ui.ask_next_archive）。
    fn play_adjacent(&mut self, frame: &mut eframe::Frame, offset: i32, ask_next: bool) {
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
                if ask_next && self.cfg.ui.ask_next_archive && self.next_archive_prompt.is_none() {
                    let list = archives_in_dir(&a.path);
                    if list.len() < 2 {
                        self.notice = Some("此目录没有其它压缩包".into());
                    } else {
                        // 默认选中循环意义上的下一个（末包绕回第一个）
                        let sel = next_archive_in_dir(&a.path)
                            .and_then(|p| list.iter().position(|q| *q == p))
                            .unwrap_or(0);
                        self.next_archive_prompt = Some(NextArchivePrompt {
                            archives: list,
                            sel,
                        });
                    }
                }
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
            // 自动翻页不触发"打开下一个压缩包"询问（到最后一张停住）
            self.play_adjacent(frame, 1, false);
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
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(v);
                                },
                            );
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
            let buffering = self.player.as_ref().map(|p| p.buffering()).unwrap_or(false);
            if buffering {
                // 主题感知的警示色：浅色主题下 YELLOW 不可读
                ui.colored_label(ui.visuals().warn_fg_color, "Buffering…");
            }
            // 解压/打开的详细进度不放顶栏：一行放不下长文件名（曾被截断），
            // 改在媒体区居中显示（见 video_area，超过宽限期还会隐藏视频窗口）。
            // 这里只在预取场景（目标 == 正在播放条目）留一个短标记：
            // 视频正被观看绝不能遮挡，无长文本不会截断
            if let Some(j) = &self.extract {
                if self.playing == Some(j.entry_idx) {
                    let left = j.pending.len();
                    ui.colored_label(
                        ui.visuals().weak_text_color(),
                        format!("后台预取中…（剩余 {left} 项）"),
                    );
                }
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
        // 高亮跟踪：playing 变化（快捷键/箭头切换）时滚动到该行（居中）。
        // 虚拟化下行可能不在渲染范围内，用行号×行高换算矩形再 scroll_to_rect
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
        egui::ScrollArea::vertical()
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
        // 行高自校正：与估计值偏差超阈值时更新（DPI/主题变化后首帧即可收敛）
        if let Some(h) = measured {
            if self.row_h.is_none() || (self.row_h.unwrap() - h).abs() > 0.01 {
                self.row_h = Some(h);
            }
        }
        if let Some(ei) = action {
            self.play(ei, frame);
        }
    }

    fn video_area(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame: &mut eframe::Frame) {
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
        // mpv 子窗口占满整个中央面板剩余区域（子窗口带 WS_EX_TRANSPARENT，鼠标事件穿透回 egui）
        let video = ui.available_rect_before_wrap();
        let child = video;

        // 视频区（原生子窗口占位）；双击=全屏（单击暂停已取消：与双击冲突，改控制栏按钮）
        let resp = ui.allocate_rect(video, egui::Sense::click());
        // 解压覆盖层显示条件（同时驱动 mpv 子窗口隐藏）：
        // - 目标 == 正在播放条目 = 预取场景：不打扰播放，走顶栏短标记
        // - 目标 ≠ 正在播放 = 用户在等新文件：
        //   · 未在播放：无画面可挡，直接显示（无闪烁问题）
        //   · 正在播 A 等 B：超过宽限期（ui.extract_overlay_delay_ms）
        //     才隐藏窗口显示覆盖层——快速解压全程不打扰 A，避免闪烁
        //   · 子窗口未创建（child_hwnd == 0）：无窗口可挡，直接显示
        let extract_wait = self.extract.as_ref().is_some_and(|j| {
            self.playing != Some(j.entry_idx)
                && (self.playing.is_none()
                    || self.child_hwnd == 0
                    || j.started.elapsed()
                        >= std::time::Duration::from_millis(self.cfg.ui.extract_overlay_delay_ms))
        });
        if extract_wait {
            // mpv 原生子窗口永远盖在 egui 之上：隐藏后覆盖层才可见
            // （与密码框/帮助面板共用机制，帧末统一同步 ShowWindow）
            self.overlay_open = true;
        }
        if extract_wait || self.playing.is_none() || self.child_hwnd == 0 {
            if let Some(j) = &self.extract {
                // 解压覆盖层：媒体区居中多行显示。顶栏一行放不下长文件名
                // （曾被截断、进度条看不见），这里空间充足：完整文件名换行 +
                // 阶段进度（solid 包跳过前置文件阶段无法算百分比 → 转圈 +
                // 已解压字节数，让用户看到"在动"）+ 写文件阶段字节进度条
                let rect = egui::Rect::from_center_size(
                    child.center(),
                    egui::vec2(child.width().min(760.0), 170.0),
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
                    egui::Rect::from_center_size(child.center(), egui::vec2(400.0, 30.0)),
                    egui::Label::new(egui::RichText::new(text).weak()).selectable(false),
                );
            }
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
                    self.play_adjacent(frame, dir, dir > 0);
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
            self.play_adjacent(frame, act.step, act.step > 0);
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
        // 启动参数打开（SendTo「Archview player」/拖到 exe 上）：首帧触发一次
        if let Some(path) = self.initial_open.take() {
            self.open_path(path);
        }
        // 拖放：压缩包文件拖入窗口即打开
        self.handle_dropped_files(&ctx);
        // 后台打开任务轮询（大包/网络盘）
        self.poll_open(frame);
        // 后台解压任务轮询（7z/RAR）
        self.poll_extract();
        // 键盘快捷键（mpv 风格）
        self.handle_shortcuts(&ctx, frame);
        // 图片自动翻页（PPT 式）
        self.slideshow_tick(&ctx, frame);
        // 密码输入弹窗（加密压缩包：加密头=打开阶段 / 加密条目=播放阶段）
        self.password_dialog(&ctx);
        // 末项后询问打开同目录下一个压缩包
        self.next_archive_dialog(&ctx);

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
        // 文件列表放左侧（视频区在右）；悬停行的完整文件名显示在
        // 面板底部状态行（方案 A），不用 hover 气泡——气泡会被右侧 mpv
        // 原生子窗口挡住（egui 感知不到子窗口，自动避让只认屏幕边界）
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
                    ShowWindow(
                        self.child_hwnd as _,
                        if self.overlay_open { SW_HIDE } else { SW_SHOW },
                    );
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

#[cfg(test)]
mod tests {
    use egui::Widget as _;

    /// 自然序排序管线（rebuild_view 同款：小写键 + alphanumeric_sort::compare_str）。
    /// 钉死语义：数字段按数值（1/2/10 而非字典序 1/10/2）、大小写不敏感、
    /// 同值前导零 tie-break（少零在前）、CJK+数字混合名按数值排。
    /// 只在同脚本组内断言相对顺序，不依赖 crate 的跨脚本比较规则
    #[test]
    fn natural_sort_pipeline_semantics() {
        fn sort(mut v: Vec<&str>) -> Vec<&str> {
            v.sort_by(|a, b| alphanumeric_sort::compare_str(&a.to_lowercase(), &b.to_lowercase()));
            v
        }
        // 纯编号：1/2/9/10/12（字典序会给出 1/10/12/2/9）
        assert_eq!(
            sort(vec!["10.jpg", "2.jpg", "1.jpg", "12.jpg", "9.jpg"]),
            vec!["1.jpg", "2.jpg", "9.jpg", "10.jpg", "12.jpg"]
        );
        // 字符+数字混合 + 大小写不敏感：IMG_2 在 img_10 前
        assert_eq!(
            sort(vec!["img_10.jpg", "IMG_2.jpg"]),
            vec!["IMG_2.jpg", "img_10.jpg"]
        );
        // CJK+数字：第2话 在 第10话 前
        assert_eq!(
            sort(vec!["第10话.jpg", "第2话.jpg"]),
            vec!["第2话.jpg", "第10话.jpg"]
        );
        // 同值前导零 tie-break：2.jpg 在 02.jpg 前（少零在前，结果稳定）
        assert_eq!(sort(vec!["02.jpg", "2.jpg"]), vec!["2.jpg", "02.jpg"]);
    }

    /// 虚拟化列表（show_viewport 手动行范围）关键行为的回归测试。
    ///
    /// 注意：__run_test_ui 默认装的是空字体（文本不换行、行高全被钳成 18），
    /// 必须显式 set_fonts 装真字体，否则测不出"长文本换行破坏等高"类问题
    /// （真实应用里 selectable_label 换行曾把行撑成 2 行高，导致渲染范围
    /// 按虚高 step 缩水 → 首页底部空隙 + 翻页时行高双稳态来回翻转）。
    #[test]
    fn virtual_list_scroll_to_row_and_fill() {
        let total = 1000usize;
        let target_row = 500usize;
        // (最终 offset, 实测行高, item_spacing, 视口高, 末行底部, 可视区底部)
        let mut result: Option<(f32, f32, f32, f32, f32, f32)> = None;
        egui::__run_test_ui(|ui| {
            ui.ctx().set_fonts(egui::FontDefinitions::default());
            let spacing = ui.spacing().item_spacing.y;
            let mut row_h: Option<f32> = None;
            for frame in 0..2 {
                let h = row_h.unwrap_or(18.0);
                // 两帧各占一块 200x300 的区域（互不重叠）；200px 宽足够让
                // 不截断的长标签换行——截断修复失效时此测试会变红
                let rect = egui::Rect::from_min_size(
                    egui::pos2(0.0, frame as f32 * 310.0),
                    egui::vec2(200.0, 300.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    let mut measured_h = 0.0f32;
                    let mut last_bottom = 0.0f32;
                    let out = egui::ScrollArea::vertical()
                        .id_salt("test_list")
                        .auto_shrink([false, false])
                        .animated(false)
                        .show_viewport(ui, |ui, viewport| {
                            let step = h + spacing;
                            ui.set_height(step * total as f32 - spacing);
                            // 与 file_list 相同的手动行范围计算（含 3 行余量）
                            let mut min_row =
                                (((viewport.min.y / step).floor() as isize - 3).max(0) as usize)
                                    .min(total);
                            let mut max_row = (viewport.max.y / step).ceil() as usize + 3;
                            if max_row > total {
                                let diff = max_row - min_row;
                                max_row = total;
                                min_row = total.saturating_sub(diff);
                            }
                            let range = min_row..max_row;
                            let y_min = ui.max_rect().top() + min_row as f32 * step;
                            let y_max = ui.max_rect().top() + max_row as f32 * step;
                            let rect =
                                egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);
                            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                                ui.skip_ahead_auto_ids(min_row);
                                if frame == 1 {
                                    // 修复后的换算：闭包 ui 的 max_rect.top 是
                                    // 内容顶 + min_row*step，行 pos 屏幕坐标 =
                                    // top + (pos - range.start) * step
                                    let y = ui.max_rect().top()
                                        + (target_row - range.start) as f32 * step;
                                    ui.scroll_to_rect(
                                        egui::Rect::from_min_size(
                                            egui::pos2(ui.max_rect().left(), y),
                                            egui::vec2(ui.available_width().max(1.0), h),
                                        ),
                                        Some(egui::Align::Center),
                                    );
                                }
                                for i in range.clone() {
                                    // 与 file_list 相同的截断渲染：若去掉 .truncate()，
                                    // 第 0 行的长标签会换行抬高行高，铺满断言变红
                                    let label = if i == 0 {
                                        "这是一个非常长的文件名，宽度超过两百像素，会把行撑成两行高"
                                            .to_string()
                                    } else {
                                        format!("[{i}] 短名")
                                    };
                                    let resp = egui::Button::selectable(i == target_row, label)
                                        .truncate()
                                        .ui(ui);
                                    if i == range.start {
                                        measured_h = resp.rect.height();
                                    }
                                    last_bottom = resp.rect.bottom();
                                }
                            });
                        });
                    if frame == 0 {
                        row_h = Some(measured_h); // 行高自校正
                    } else {
                        result = Some((
                            out.state.offset.y,
                            measured_h,
                            ui.spacing().item_spacing.y,
                            300.0,
                            last_bottom,
                            out.inner_rect.bottom(),
                        ));
                    }
                });
            }
        });
        let (offset, natural_h, spacing, view_h, last_bottom, view_bottom) =
            result.expect("应完成两帧");
        // 说明：测试上下文没有真实字体数据（文本不换行、行高恒 18），
        // "换行破坏等高"无法在此复现，靠上面的 truncate 调用 + 真实应用验证；
        // 此处断言保留坐标换算与铺满行为的覆盖
        assert!(
            natural_h < 26.0,
            "截断后行高应约 18px（实测 {natural_h}），超过则长标签仍在换行"
        );
        // 期望：目标行居中 → offset ≈ 行号×步长 + 行高/2 - 视口/2（±一行内即可）
        let step = natural_h + spacing;
        let expected = target_row as f32 * step + natural_h * 0.5 - view_h * 0.5;
        assert!(
            (offset - expected).abs() < step,
            "滚动 offset={offset} 应接近居中期望 {expected}（step={step}）"
        );
        // 铺满：行高校正后，渲染的最后一行底边应盖到可视区底部（留一行容差）
        assert!(
            view_bottom - last_bottom < step,
            "末行底部 {last_bottom} 距可视区底 {view_bottom} 超过一行，列表未铺满"
        );
    }

    /// 高亮追踪滚动回归测试（用户报告的 BUG）：
    /// 列表已手动滚动到远处（~200 行），此时 playing 切到 51 行触发
    /// scroll_to_rect 高亮居中——列表应回到 51 行附近，而不是跳到底部。
    ///
    /// 多帧多 pass 模拟真实时序（滚轮事件 → 动画收敛），区别于上面的
    /// 单 pass 两帧测试：这里 ScrollArea 是默认 animated（应用未关闭动画）。
    #[test]
    fn virtual_list_track_scroll_from_manually_scrolled_position() {
        let total = 10_000usize;
        let target = 51usize;
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(400.0, 300.0),
        ));
        let mut row_h: Option<f32> = None;
        let mut offsets: Vec<f32> = Vec::new();
        let mut scroll_to = false;

        let mut t = 0.0;
        for frame in 0..160 {
            t += 0.016;
            raw.time = Some(t);
            if (5..45).contains(&frame) {
                // 鼠标悬停列表，滚轮向下翻（看后面的行）
                raw.events
                    .push(egui::Event::PointerMoved(egui::pos2(100.0, 150.0)));
                raw.events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, -8.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::default(),
                });
            } else if frame >= 45 {
                // 鼠标移到列表外（模拟回到视频区滚轮翻页；列表不 hover，
                // 滚轮惯性尾巴才不会每帧清掉 scroll_to_rect 的动画目标）
                raw.events
                    .push(egui::Event::PointerMoved(egui::pos2(2000.0, 150.0)));
            }
            if frame == 50 {
                // 模拟 playing 变化触发高亮滚动（视频区滚轮翻页 → 解压完成 → playing 更新）
                scroll_to = true;
            }
            let output = ctx.run_ui(raw.take(), |ui| {
                let h = row_h.unwrap_or(18.0);
                let spacing = ui.spacing().item_spacing.y;
                let step = h + spacing;
                let mut measured = 0.0f32;
                let out = egui::ScrollArea::vertical()
                    .id_salt("track_list")
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, viewport| {
                        ui.set_height(step * total as f32 - spacing);
                        let mut min_row = (((viewport.min.y / step).floor() as isize - 3).max(0)
                            as usize)
                            .min(total);
                        let mut max_row = (viewport.max.y / step).ceil() as usize + 3;
                        if max_row > total {
                            let diff = max_row - min_row;
                            max_row = total;
                            min_row = total.saturating_sub(diff);
                        }
                        let range = min_row..max_row;
                        let y_min = ui.max_rect().top() + min_row as f32 * step;
                        let y_max = ui.max_rect().top() + max_row as f32 * step;
                        let rect =
                            egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);
                        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                            ui.skip_ahead_auto_ids(min_row);
                            if scroll_to {
                                // 与 file_list 相同的换算（isize 防下溢 wrap）
                                let y = ui.max_rect().top()
                                    + (target as isize - range.start as isize) as f32 * step;
                                ui.scroll_to_rect(
                                    egui::Rect::from_min_size(
                                        egui::pos2(ui.max_rect().left(), y),
                                        egui::vec2(ui.available_width().max(1.0), h),
                                    ),
                                    Some(egui::Align::Center),
                                );
                            }
                            for i in range.clone() {
                                let resp =
                                    egui::Button::selectable(i == target, format!("[{i}] 短名"))
                                        .truncate()
                                        .ui(ui);
                                if i == min_row {
                                    measured = resp.rect.height();
                                }
                            }
                        });
                    });
                if row_h.is_none() && measured > 0.0 {
                    row_h = Some(measured);
                }
                offsets.push(out.state.offset.y);
            });
            output.drop_without_applying_deltas();
            scroll_to = false;
        }

        let step = row_h.unwrap_or(18.0) + 3.0; // item_spacing.y 默认 3
        let before = offsets[49]; // 触发滚动前
        let final_offset = *offsets.last().unwrap();
        assert!(
            before > 100.0 * step,
            "前置条件不满足：滚轮后 offset={before} 应已滚到 100 行以外"
        );
        let expected = target as f32 * step + row_h.unwrap_or(18.0) * 0.5 - 150.0;
        assert!(
            (final_offset - expected).abs() < 5.0 * step,
            "高亮追踪失败：最终 offset={final_offset}，期望 ≈{expected}（target 居中），\
             触发前 offset={before}，step={step}，轨迹尾部={:?}",
            &offsets[140..]
        );
    }
}
