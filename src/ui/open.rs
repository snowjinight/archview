//! 打开链路：文件/目录对话框、拖放入口、密码分支、后台打开任务轮询与视图重建。

use super::*;

impl App {
    // ---- 打开压缩包 ----

    pub(super) fn open_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Archives", &["zip", "7z", "rar", "iso"])
            .pick_file()
        else {
            return;
        };
        self.open_path(path);
    }

    /// 打开目录按钮：目录即媒体集合（递归收集图片/视频，跳过压缩包——
    /// 与拖放/命令行参数同一条规则，doc/架构设计/04_VFS层.md §4.2）。
    /// 空集合提示在打开完成时统一给出（poll_open 的 Done 分支）
    pub(super) fn open_dir_dialog(&mut self) {
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        self.open_path(dir);
    }

    /// 「文件 → 打开媒体文件…」：选中单个媒体文件 → 打开其所在目录的
    /// 媒体集合并跳转播放该文件（与拖入媒体文件同一条分流，见 open_path）
    pub(super) fn open_media_dialog(&mut self) {
        // 过滤器 = 视频+图片扩展名并集（config 驱动，排序去重保证稳定）
        let mut exts: Vec<&str> = self
            .cfg
            .media
            .video_extensions
            .iter()
            .chain(self.cfg.media.image_extensions.iter())
            .map(|s| s.as_str())
            .collect();
        exts.sort_unstable();
        exts.dedup();
        let Some(path) = rfd::FileDialog::new()
            .add_filter("媒体文件", &exts)
            .pick_file()
        else {
            return;
        };
        self.open_path(path);
    }

    /// 处理本帧拖入的文件（egui dropped_files）：取第一个路径打开——
    /// 压缩包开包、目录开媒体集合（同一条规则，见 spawn_open 目录分支）
    pub(super) fn handle_dropped_files(&mut self, ctx: &egui::Context) {
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

    pub(super) fn open_path(&mut self, path: std::path::PathBuf) {
        // 单媒体文件入口（拖入文件/SendTo/命令行/「打开媒体文件」）：文件
        // 本身不是打开目标，改为打开其所在目录的媒体集合，并记下目标文件
        // 名（pending_focus，打开完成时消费：预切标签页 + 跳转播放）。
        // 每次打开先清零，防上一次的焦点串到本次
        self.pending_focus = None;
        let path = if path.is_file() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if self.cfg.is_video_ext(&name) || self.cfg.is_image_ext(&name) {
                self.pending_focus = Some(name);
                match path.parent() {
                    Some(p) => p.to_path_buf(),
                    None => path,
                }
            } else {
                path
            }
        } else {
            path
        };
        // 会话内密码记忆：本会话输过密码的包直接带密码打开（不再弹框）
        let cached = self.pw_memory.get(&path).cloned();
        self.open_path_pw(path, cached.as_deref());
    }

    /// 打开压缩包；Password 错误（加密头）时弹密码框，提交后带密码重开。
    /// 打开+列表在后台线程执行（大包可达分钟级，不能冻结 UI）
    pub(super) fn open_path_pw(&mut self, path: std::path::PathBuf, password: Option<&str>) {
        self.spawn_open(path, password.map(str::to_string), None);
    }

    /// 起后台线程打开压缩包（含 stream_cb 注册/列表，均为耗时步骤）。
    /// gen 与解压任务共用 next_gen：快速连续切换时旧结果直接丢弃
    pub(super) fn spawn_open(
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
        // 目录集合的媒体名谓词（is_video_ext ∨ is_image_ext，与 rebuild_view
        // 同源）：标准扩展名 ∨ [[raw.demux]] 预设 glob 命中——.yuv420 等非标
        // 名在收集层即可见（旧「扩展名白名单」旁路曾把预设命中文件在扫描
        // 阶段丢弃，目录模式下列表里根本不出现）；压缩包等其它文件天然排除
        let cfg_t = self.cfg.clone();
        // 排序开关快照（后台预排序判据，与 rebuild_view 同源：FastSeek 强制
        // 自然序 ∨ ui.sort_files）；布尔拷贝避免 cfg_t 被 is_media 闭包 move
        let sort_files = self.cfg.ui.sort_files;
        let is_media = move |name: &str| cfg_t.is_video_ext(name) || cfg_t.is_image_ext(name);
        // 单媒体文件入口（pending_focus 置位）对所在目录的收集深度取配置
        // （默认 1 = 只收集目标文件同级，防盘根/超大目录全盘扫描）；
        // 显式打开目录不限深（与既有递归行为一致）
        let dir_depth = if self.pending_focus.is_some() {
            self.cfg.media.file_open_depth
        } else {
            0
        };
        let spawned = std::thread::Builder::new()
            .name("open_archive".into())
            .spawn({
                // 消息 send 后主动唤醒 UI 立即 poll：空闲时 UI 不再有心跳
                // 排帧（全事件驱动），进度显示与终态套用全靠此唤醒
                let egui_ctx = self.egui_ctx.clone();
                move || {
                    // 阶段 1（目录递归扫描 / crate 内部解析头/CD，无法细分）：不定进度
                    let stage = if path_t.is_dir() {
                        "正在扫描目录"
                    } else {
                        "正在读取压缩包目录"
                    };
                    let _ = tx.send(OpenMsg::Progress {
                        fraction: None,
                        stage: stage.into(),
                    });
                    egui_ctx.request_repaint();
                    // 目录模式：目录即媒体集合（DirVFS 递归收集图片/视频，跳过压缩包，
                    // doc/架构设计/04_VFS层.md §4.2）——无加密概念，不走密码探测链；文件模式照旧
                    let opened = if path_t.is_dir() {
                        DirVFS::open(&path_t, is_media, dir_depth)
                            .map(|v| (Box::new(v) as Box<dyn ArchiveVFS>, None))
                    } else {
                        // 打开阶段即探测并锁定密码（doc/架构设计/12_VarTidy整理器.md §12.5）：
                        // 加密头 open 即验；内容加密对最小加密条目逐候选校验。
                        // 探测在后台线程完成，错密码不会替换已打开状态，直接重弹
                        open_with_password_probe(&path_t, &candidates)
                    };
                    let msg = match opened {
                        Ok((vfs, locked)) => {
                            let arc: Arc<dyn ArchiveVFS> = vfs.into();
                            // 阶段 2（list，zip 的 CD 顺序遍历有真实百分比）
                            let (id, entries) =
                                stream_cb::register_with_progress(arc.clone(), &path_t, &|f, s| {
                                    let _ = tx.send(OpenMsg::Progress {
                                        fraction: f,
                                        stage: s.to_string(),
                                    });
                                    // 进度回调由 VFS 层节流（zip 每 256 条目
                                    // 1 次，zip_vfs.rs list_from_central_directory），
                                    // 频率极低；request_repaint 仅置内部标志，
                                    // 开销可忽略
                                    egui_ctx.request_repaint();
                                });
                            // 后台预排序：排序必须等全部条目收集完才能开始
                            // （需要完整 name 域），紧跟收集串行执行——藏在打开
                            // 耗时阶段里，UI rebuild_view 只按基序过滤零排序
                            // （70 万条实测 1.3s，同步排序曾冻结 UI 线程）。
                            // 固态包关闭排序时走恒等序（=物理序），只建索引
                            // 不动 entries，下游预取批/上游预取相位行为不变
                            let natural =
                                arc.access_mode() == AccessMode::FastSeek || sort_files;
                            let (disp_order, disp_natural) = if natural {
                                // 排序中状态：大包排序秒级可见（70 万条 1.3s），
                                // 进度区从"正在读取压缩包目录 100%"切换为排序文案
                                let _ = tx.send(OpenMsg::Progress {
                                    fraction: None,
                                    stage: "正在排序文件列表".into(),
                                });
                                egui_ctx.request_repaint();
                                let t = std::time::Instant::now();
                                // 分块排序 + 进度回报：键生成/块排序/多路归并
                                // 各阶段持续上报 fraction，大包排序全程有
                                // 进度可见而非黑箱转圈
                                let order = natural_order_with_progress(&entries, &|f| {
                                    let _ = tx.send(OpenMsg::Progress {
                                        fraction: Some(f.clamp(0.0, 1.0) as f32),
                                        stage: "正在排序文件列表".into(),
                                    });
                                    egui_ctx.request_repaint();
                                });
                                // 排序完成状态：随后 Done 到达、下一帧列表出现
                                // 即自然消失（无人工延时）
                                let _ = tx.send(OpenMsg::Progress {
                                    fraction: None,
                                    stage: "排序完成".into(),
                                });
                                egui_ctx.request_repaint();
                                // debug 级排序耗时探针（大包排序成本观测用）
                                tracing::debug!(
                                    "自然序预排序 {} 条耗时 {:?}",
                                    entries.len(),
                                    t.elapsed()
                                );
                                (Some(order), true)
                            } else {
                                (None, false) // 恒等序由 apply_open_done 补齐
                            };
                            OpenMsg::Done {
                                vfs: arc,
                                id,
                                entries,
                                pw: locked,
                                disp_order,
                                disp_natural,
                            }
                        }
                        Err(VfsError::Password) => OpenMsg::NeedPw {
                            wrong: pw.is_some(),
                        },
                        Err(e) => {
                            OpenMsg::Failed(format!("Failed to open {}: {e}", path_t.display()))
                        }
                    };
                    let _ = tx.send(msg);
                    egui_ctx.request_repaint();
                }
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
    pub(super) fn poll_open(&mut self) {
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
                    disp_order,
                    disp_natural,
                } => {
                    let to_play = self.apply_open_done(
                        job.path,
                        job.retry_play,
                        vfs,
                        id,
                        entries,
                        pw,
                        disp_order,
                        disp_natural,
                    );
                    // 密码重试场景：恢复播放提交密码时想看的条目
                    if let Some(entry_idx) = to_play {
                        self.play(entry_idx);
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

    /// 应用打开终态（Done）的状态迁移：建 ArchiveState、重建视图、清旧任务。
    /// 返回需立即播放的条目下标（密码重试场景）——poll_open 收到终态后执行
    /// play；独立成方法是因为单测造不出完整打开链路（见 dir_mode 测试）
    pub(super) fn apply_open_done(
        &mut self,
        path: PathBuf,
        retry_play: Option<(usize, VirtualEntry)>,
        vfs: Arc<dyn ArchiveVFS>,
        id: u64,
        entries: Vec<VirtualEntry>,
        pw: Option<String>,
        // Some = 自然序基序（后台线程预排序产物）；None = 恒等序（测试
        // 旁路/物理序，此处补齐 0..n）
        disp_order: Option<Vec<usize>>,
        disp_natural: bool,
    ) -> Option<usize> {
        // 登记进任务栏「最近」列表：Shell 只自动登记文件关联启动，拖入/
        // SendTo/目录入口一律漏掉——打开成功即补登（见 recent_docs 模块文档）
        crate::recent_docs::add(&path);
        // 切换目录自动保存：旧目录还有待同步评级 → 立即启动同步 worker。
        // worker 持有的是排空后的 (PathBuf, 值) 列表，与新会话零耦合（异步
        // 落盘，新目录加载与写入并行）；dirty 只在目录模式产生（压缩包模式
        // 评级一律会话内），守卫在压缩包/密码重试路径上恒为空操作
        if !self.rating_dirty.is_empty() {
            self.start_rating_sync(false);
        }
        // 切换压缩包：按配置清理上一个包的解压缓存（默认不清理——保留可命中）
        if self.cfg.cache.clean_on_switch {
            if let Some(old) = &self.archive {
                let freed = cache::clean_archive(&cache::cache_dir(&self.cfg), old.id);
                if freed > 0 {
                    tracing::info!("切换压缩包，清理旧缓存 {:.1} MB", freed as f64 / 1048576.0);
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
        // 缓存命中位图建图：一次 readdir 替代此后批扫描的全部逐条目 metadata
        // （本包上次会话遗留的落地文件，按 {id}_{idx}_ 前缀解析 + 尺寸校验）
        let cached = cache::scan_cached(&cache::cache_dir(&self.cfg), id, &entries);
        // 恒等序补齐（None = 物理序原样，一次性 0..n 构造代价可忽略）
        let disp_order = disp_order.unwrap_or_else(|| (0..entries.len()).collect());
        self.archive = Some(ArchiveState {
            id,
            name,
            path: path.clone(),
            password: pw.clone(),
            entries,
            view: Vec::new(),
            vfs,
            stream,
            // 全新会话：alive=false 起步，首次 play 组批后置 true
            // 并 spawn 解码线程（协议见 vfs::ArchiveVFS::extract_session）
            session: Arc::new(std::sync::Mutex::new(SessionState {
                targets: Default::default(),
                focus: None,
                alive: false,
                gen: 0,
                skip_note: None,
                resume: false,
            })),
            ratings: std::collections::HashMap::new(),
            deleted: std::collections::HashSet::new(),
            cached,
            // 目录模式判定一次性缓存（每帧 stat 在网络盘上是 ~70ms 级往返，
            // 见 ArchiveState.dir_mode 注释）
            dir_mode: path.is_dir(),
            disp_order,
            disp_is_natural: disp_natural,
        });
        // 新包旧选择无意义：清多选与范围锚点（entry 下标对新包是错位引用）
        self.selected.clear();
        self.select_anchor = None;
        // 星级筛选/删除确认窗是 per-archive 会话状态，换包即清
        self.rating_filter = None;
        self.delete_confirm = None;
        self.rebuild_view();
        // 目录模式：按需评级探测（视口惰性，取代旧一次性全量扫描——万级
        // 目录打开即全量扫是纯浪费，评级的消费者只有列表亮星与星级筛选）。
        // 打开时只登记图片条目下标快照 + 常驻 worker，探测批次由列表视口
        // 钩子按需入队（panels.rs debounce 后调 rating_probe_enqueue，首帧
        // 空集立即入队头部窗口）。压缩包模式无磁盘路径不探测（会话内评级）。
        // 评级按分格式真相矩阵读取（meta.rs 头注）；换包 = drop job + 调度器
        // cancel(Rating)：旧 rx 随句柄丢弃（迟到结果不串包）
        // 换包 = 评级探测会话重置（job drop + 调度器 cancel(Rating) 清排队
        // 任务，迟到结果不串包——见 rating_probe_session_reset）
        self.rating_probe_session_reset();
        // 换包清排队请求：旧包的排队任务对新集合是陈旧意图（key 含
        // archive_id 不会误发，但占队列无意义）
        self.img.clear_queued();
        if path.is_dir() {
            // 位图按 entries.len() 分配（idx 致密域：下标直寻去重）
            self.rating_probed = vec![false; self.archive.as_ref().unwrap().entries.len()];
            self.rating_image_idx = self
                .archive
                .as_ref()
                .unwrap()
                .entries
                .iter()
                .enumerate()
                // sidecar 评级全格式可用（含 RAW/GIF/BMP）→ 不限
                // meta_writable，按图片扩展名全集取样（筛选补扫全集）
                .filter(|(_, e)| !e.is_dir && self.cfg.is_image_ext(&e.name))
                .map(|(i, _)| i)
                .collect();
            // 探测 = IoJob 直接提交调度器 Rating 类队列（读线程读毕
            // on_done 回投结果，设计 §19 D8），无 worker 线程；job 只持
            // 结果通道（tx 供 enqueue 闭包回投，rx 由 poll 排空）
            let (tx, rx) = std::sync::mpsc::channel::<(usize, Option<u8>)>();
            self.rating_probe = Some(RatingProbeJob { tx, rx });
        }
        // 单媒体文件入口：目标文件所在目录打开完成（仅目录模式消费——
        // 压缩包路径不吃焦点，防"焦点未消费又被压缩包打开覆盖"的竞态串播）。
        // 按目标扩展名预切标签页：混合内容目录下不预切的话目标在另一
        // 标签页里，跳转落空；已在该页则不再多算一次行缓存
        let focus = if path.is_dir() {
            self.pending_focus.take()
        } else {
            None
        };
        if let Some(name) = &focus {
            let want = if self.cfg.is_image_ext(name) {
                Some(ListTab::Image)
            } else if self.cfg.is_video_ext(name) {
                Some(ListTab::Video)
            } else {
                None
            };
            let has_items = |t: ListTab| match t {
                ListTab::Image => self.list_counts.1 > 0,
                ListTab::Video => self.list_counts.0 > 0,
            };
            if let Some(t) = want.filter(|t| *t != self.list_tab && has_items(*t)) {
                self.list_tab = t;
                self.rebuild_view();
            }
        }
        let what = if path.is_dir() {
            "打开目录媒体集合"
        } else {
            "打开压缩包"
        };
        tracing::info!(
            "{what}: {}（{} 项）",
            path.display(),
            self.archive.as_ref().map(|a| a.entries.len()).unwrap_or(0)
        );
        // 切换内容源 = 翻篇：先清掉旧包遗留的红/绿消息（须在空集合判断
        // 之前，否则下面 set_notice 会被清掉）
        self.error = None;
        self.notice = None;
        // 空集合提示（目录里没有媒体 / 压缩包内没有媒体文件）——
        // 统一在此给出，各打开入口不必各自判断
        if self.archive.as_ref().is_some_and(|a| a.view.is_empty()) {
            self.set_notice("没有可播放的视频/图片");
        }
        // 取消并清掉旧包的解压任务：不清会导致旧包 FileDone 到来时
        // load_extracted 把画面切回旧包条目（playing 下标错乱）
        if let Some(j) = &self.extract {
            tracing::info!("取消解压任务: {}（切换压缩包）", j.entry_name);
            j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.extract = None;
        self.playing = None;
        self.last_focus = None; // 旧包的锚点下标对新包无意义
                                // 对比会话随旧包失效（栏键 = (archive_id, idx)）：清会话 + 多键
                                // 驻留保护 + raw 帧缓存，防止新包渲染引用旧包键
        if self.compare.take().is_some() {
            self.img.clear_shown_multi();
            self.rawdec.retain(&[]);
        }
        self.pw_prompt = None;
        // 密码重试场景：恢复播放提交密码时想看的条目（返回调用方执行）；
        // 单媒体文件入口：跳转播放用户传入的目标文件（poll_open 执行 play，
        // playing 置位后列表高亮滚动居中由既有机制接管）。名字比对不区分
        // ASCII 大小写（Windows 文件系统不区分，收集名与拖入名可能不同写法）；
        // 找不到同名条目则静默不播（视图照常打开）
        let focus_idx = focus.and_then(|name| {
            self.archive
                .as_ref()?
                .entries
                .iter()
                .position(|e| e.name.eq_ignore_ascii_case(&name))
        });
        retry_play.map(|(entry_idx, _)| entry_idx).or(focus_idx)
    }

    /// 按需评级探测入队（panels.rs 视口钩子与筛选补扫共用）：**mark-at-enqueue**
    /// ——入队瞬间 `rating_probed[idx]` 置位（false→true 才真正提交），同一
    /// idx 在生命周期内结构上不可能二次入队；无独立 pending 集，入队调用方
    /// 只在窗口变化/筛选激活等离散事件发生，不逐帧去重。单条去重成本 =
    /// 一次位图下标读写（零哈希零扫描，与已标记量无关）。提交 = IoJob 直接
    /// 进调度器 Rating 队列（优先级最低，读线程按队列序消费，设计 §19 D8）。
    /// job 缺位（压缩包模式）零动作
    pub(super) fn rating_probe_enqueue(&mut self, items: RatingProbeBatch) {
        if self.rating_probe.is_none() {
            return;
        }
        let mut pushed = 0usize;
        for (idx, p, name) in items {
            if idx < self.rating_probed.len() && !self.rating_probed[idx] {
                self.rating_probed[idx] = true;
                self.submit_rating_probe(idx, p, name);
                pushed += 1;
            }
        }
        if pushed > 0 {
            // info 批级转折（队列深度 = 调度器 Rating 队列观测点；idx 非连续
            // 属正常，视图行映射的是图片条目下标，目录行/视频条目穿插其间）
            tracing::info!(
                "评级探测入队 {} 项（队列 {}）",
                pushed,
                self.io.queued(IoClass::Rating),
            );
        }
    }

    /// 单条评级探测提交：读段 = meta::read_rating（评级头读，NAS 上毫秒级），
    /// IoJob 入调度器 Rating 队列，读毕 on_done 回投 (idx, 星级) 并唤醒 UI。
    /// 无记账无看门狗——读卡死由调度器侧看门狗救援（设计 §19 D5/D6）
    fn submit_rating_probe(&mut self, idx: usize, p: PathBuf, name: String) {
        let job = self.rating_probe.as_ref().expect("enqueue 已判 job 在位");
        let tx = job.tx.clone();
        let ctx = self.egui_ctx.clone();
        let work = {
            let p = p.clone();
            let name = name.clone();
            Box::new(move || {
                Ok::<Vec<u8>, String>(
                    crate::meta::read_rating(&p, &name).map(|v| vec![v]).unwrap_or_default(),
                )
            })
        };
        let on_done = Box::new(move |r: Result<Vec<u8>, String>| {
            let _ = tx.send((idx, r.ok().and_then(|b| b.first().copied())));
            ctx.request_repaint();
        });
        self.io.submit(IoJob {
            name: format!("评级 {name}"),
            class: IoClass::Rating,
            token: 0,
            heavy: false,
            work,
            on_done,
        });
        // 集齐判定计数：提交一计（poll 消费回执一计，计数归等 = 全部回投）
        self.rating_submitted += 1;
    }

    /// 评级探测会话重置（换包唯一入口）：全部探测状态清零 + 调度器
    /// cancel(Rating) 清排队任务（取消语义，设计 §19 D8）；job drop 后
    /// 旧 rx 随句柄丢弃，在途读毕的迟到结果不串包
    fn rating_probe_session_reset(&mut self) {
        self.io.cancel(IoClass::Rating);
        self.rating_probe = None;
        self.rating_probed = Vec::new();
        self.rating_probe_window = None;
        self.rating_probe_intent = None;
        self.rating_image_idx = Vec::new();
        self.rating_submitted = 0;
        self.rating_done = 0;
    }

    /// 视口窗口入队便捷层：entry 下标切片 → (idx, 路径, 文件名) 探测项。
    /// 视口钩子（panels.rs）与筛选补扫共用；archive 缺位零动作
    pub(super) fn rating_probe_enqueue_rows(&mut self, idxs: &[usize]) {
        let Some(a) = self.archive.as_ref() else {
            return;
        };
        let dir = a.path.clone();
        let items: RatingProbeBatch = idxs
            .iter()
            .filter_map(|&i| {
                a.entries
                    .get(i)
                    .map(|e| (i, dir.join(&e.name), e.name.clone()))
            })
            .collect();
        self.rating_probe_enqueue(items);
    }

    /// 星级筛选激活补扫：遍历打开时登记的图片条目快照，enqueue 的
    /// insert-noop 天然只入未探测的剩余项（已入队/已完成的不会重发）
    /// ——"筛选只扫剩下没扫过的"。压缩包模式快照为空零动作
    pub(super) fn rating_probe_scan_rest(&mut self) {
        let idxs: Vec<usize> = self.rating_image_idx.clone();
        self.rating_probe_enqueue_rows(&idxs);
    }

    /// 按需评级探测轮询：收当帧全部结果，合并进 a.ratings（**会话值
    /// 优先**——用户探测期间手动评的星不被覆盖）。探测任务全部由
    /// submit_rating_probe 提交调度器（读线程读毕 on_done 回投），本函数
    /// 只消费结果通道；调度器 queued(Rating) 归零且星级筛选已选时重建
    /// 视图（筛选谓词等全量事实就绪才生效，防列表中途反复闪变）
    pub(super) fn poll_rating_probe(&mut self) {
        let mut got = false;
        loop {
            let Some(job) = self.rating_probe.as_ref() else {
                break;
            };
            match job.rx.try_recv() {
                Ok((idx, r)) => {
                    got = true;
                    // 集齐判定计数：回执消费一计（失败回执同样计——读失败
                    // 也回投消息，计数必然归等收敛）
                    self.rating_done += 1;
                    let Some(a) = self.archive.as_mut() else {
                        break;
                    };
                    if let Some(r) = r {
                        a.ratings.entry(idx).or_insert(r);
                    }
                }
                Err(_) => break, // Empty：收尽；Disconnected：rx 随 job 摘除即终
            }
        }
        // 批级转折（info）：整批探测完成（F12 可见的评级日志就这一条 +
        // 入队一条，中间过程 debug）。提交/完成计数归等 = 全部结果已回投
        // 或随换包 cancel 清账，事实集齐，筛选可安全生效（比 queued(Rating)
        // 归零准：读毕出队后结果还在路上时队列深度已漏算这一段）
        if got
            && self.rating_done == self.rating_submitted
            && self.rating_filter.is_some()
        {
            tracing::info!("评级探测完成（本轮队列已清空）");
            self.rebuild_view();
        }
    }

    /// 重新计算过滤视图：搜索框高级语法匹配（search_engine：空格 AND、| OR、
    /// ! 排除、"精确短语"、* ? 通配、<分组>，语法说明见根目录 serach.md）
    /// + 只显示媒体文件（视频/图片按 config 分类），按打开后台线程预排序的
    /// 显示基序遍历（disp_order：自然序或恒等物理序，排序成本已藏在打开
    /// 阶段，此处零排序；基序失配兜底见下）。
    /// 同时重建列表行缓存（虚拟化渲染的显示数据）：只在触发条件变化时执行
    /// （打开/搜索过滤/切标签/配置重载改排序或扩展名分类/评级筛选），
    /// 不再每帧重算——万级条目时每帧 clone/format 是 UI 卡顿的主因
    pub(super) fn rebuild_view(&mut self) {
        // 本重建即消费当前搜索文本，脏标记统一清零（防抖到期/打开/切标签
        // 等任何入口调用后，file_list 不再重复触发）
        self.search_dirty = false;
        let Some(a) = &mut self.archive else {
            self.list_rows.clear();
            self.list_counts = (0, 0);
            return;
        };
        // 解析一次，逐条目评估：无通配符的纯词匹配内部退化为 contains，
        // 性能与旧子串实现持平；空查询 matches 一切（等价于不过滤）。
        // 视频判定（is_video_ext 含 raw 预设匹配）每条目只做一次，分类结果
        // kinds 与 view 同序携带，后续计数/行缓存直接查表——旧实现同一名单
        // 要过 3 遍 is_video_ext，万级条目 × raw 预设回溯时开销放大 3 倍
        let query = crate::search::parse_query(&self.search);
        // 星级筛选谓词（仅图片条目；与文本搜索 AND）：Some(0)=未评、
        // Some(n)=≥n 星。探测计数未归等（rating_submitted > rating_done，
        // 含在途读）时暂不生效（poll_rating_probe 归等后重建视图）——
        // 防列表中途反复闪变。已删除条目（trash 删除后的会话标记）无条件剔除
        let filter_active = self.rating_submitted == self.rating_done
            && self.rating_filter.is_some();
        let rating_filter = self.rating_filter;
        // 基序失配兜底（config 热重载切换 sort_files 的罕见路径）：打开后台
        // 线程预排序用的开关快照与当前配置不符时就地修正 disp_order——
        // 补排序/重置恒等序一次，此后 disp_order 与当前需求重新对齐。
        // 正常路径（打开后未改配置）两个分支都不命中，零开销直取基序
        let needed = a.stream || self.cfg.ui.sort_files;
        if needed && !a.disp_is_natural {
            let t = std::time::Instant::now();
            let mut keyed: Vec<(usize, String)> = a
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| (i, e.name.to_lowercase()))
                .collect();
            keyed.sort_by(|x, y| alphanumeric_sort::compare_str(&x.1, &y.1));
            a.disp_order = keyed.into_iter().map(|(i, _)| i).collect();
            a.disp_is_natural = true;
            tracing::debug!("热重载补排序 {} 条耗时 {:?}", a.entries.len(), t.elapsed());
        } else if !needed && a.disp_is_natural {
            a.disp_order = (0..a.entries.len()).collect();
            a.disp_is_natural = false;
        }
        // 按基序遍历收集（打开后台线程已预排序；恒等序时 = 物理序原样）。
        // 排序语义与判据同 spawn_open：排序键 = 完整相对路径（name 含子目录
        // 路径）：同前缀子目录的条目天然连成一片（原则 1），目录名与夹内
        // 文件名的数字段都按数值排（原则 2，第1话/1.jpg → 第1话/2.jpg →
        // 第2话/1.jpg → 第10话/1.jpg）。FastSeek 包（ZIP/ISO/目录集合，
        // stream=true）**强制**自然序：无解压会话，图片预取走 Stream 源随机
        // 读、视频走 mpv stream_cb 随机 seek，物理序零收益，排序纯观感；
        // 解压包（7z/RAR，solid 与 single-file 皆同）按 config: ui.sort_files
        // （默认关 = 保持压缩包内部顺序：固态包下内部顺序 == 物理码流顺序，
        // 下游预取批恰好是物理相邻的后续文件，翻页直接命中缓存，性能最优）。
        // 注意 7-Zip 工具打的包内部本来就是字典序（实验验证：无视添加顺序
        // 强制重排），关闭排序对这类包的显示无变化，属正常现象
        let classified: Vec<(usize, bool)> = a
            .disp_order
            .iter()
            .filter_map(|&i| {
                let e = &a.entries[i];
                if e.is_dir || !query.matches(&e.name) || a.deleted.contains(&i) {
                    return None;
                }
                let is_video = self.cfg.is_video_ext(&e.name);
                if !is_video && filter_active {
                    let r = a.ratings.get(&i).copied().unwrap_or(0);
                    let want = rating_filter.unwrap();
                    if (want == 0 && r != 0) || (want > 0 && r < want) {
                        return None;
                    }
                }
                (is_video || self.cfg.is_image_ext(&e.name)).then_some((i, is_video))
            })
            .collect();
        let (view, kinds): (Vec<usize>, Vec<bool>) = classified.into_iter().unzip();
        a.view = view;

        // 自动切标签页：当前标签页为空而另一类非空时跳过去。
        // 纯图片/纯视频的包打开时不再停在空列表上让用户误以为没打开
        // （搜索过滤后的空结果不切——那属于"没匹配"而非"没这类文件"，
        //   需要搜索框为空才切，避免打断用户在另一标签页里搜东西；
        //   注意不能用 query.is_empty()：输入 "!" 等无法成词的内容也会解析为空树）
        if self.search.trim().is_empty() {
            let (n_video, n_image) = kinds.iter().fold((0usize, 0usize), |(v, m), &is_video| {
                if is_video {
                    (v + 1, m)
                } else {
                    (v, m + 1)
                }
            });
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

        // 行缓存：当前标签页的行 + 两类计数（view 内，分类查 kinds 不再重判）
        let mut counts = (0usize, 0usize);
        let mut tab_items: Vec<(usize, &str)> = Vec::new();
        for (&i, &is_video) in a.view.iter().zip(&kinds) {
            if is_video {
                counts.0 += 1;
            } else {
                counts.1 += 1;
            }
            // 只收集当前标签页的行
            if (self.list_tab == ListTab::Video) != is_video {
                continue;
            }
            tab_items.push((i, a.entries[i].name.as_str()));
        }
        let total = tab_items.len();
        self.list_rows = tab_items
            .into_iter()
            .enumerate()
            .map(|(n, (idx, name))| {
                // 行文本只显示文件名（目录段在悬停气泡双色展示）：内部键、排序、
                // 播放/解压全走 entries[idx].name 完整路径，此处仅显示层剥离。
                // 换行符清洗成空格：内嵌 \n 会把行撑成多行高，破坏虚拟化等高前提
                let file = name.rsplit_once('/').map(|(_, f)| f).unwrap_or(name);
                ListRow {
                    idx,
                    label: format!("[{}/{}] {}", n + 1, total, file).replace(['\n', '\r'], " "),
                }
            })
            .collect();
        self.list_counts = counts;
    }
}

/// 自然序预排序（分块 + 进度回报）：排序必须等全部条目收集完才能开始
/// （需要完整 name 域），在打开后台线程执行。三阶段——分块键生成 / 分块
/// 内部排序 / 多路归并——各自回报进度（fraction 0~1）。结果等价于整体
/// 稳定排序：键相同（仅大小写差异）时按原始下标序，与 `sort_by` 稳定
/// 语义一致。大包（70 万条实测 1.6s）排序全程有进度而非黑箱。
fn natural_order_with_progress(
    entries: &[crate::vfs::VirtualEntry],
    report: &dyn Fn(f64),
) -> Vec<usize> {
    use std::cmp::{Ordering, Reverse};
    use std::collections::BinaryHeap;

    const CHUNK: usize = 65536;
    let n = entries.len();
    if n == 0 {
        return Vec::new();
    }
    // 阶段 1：小写键预生成（每条一次字符串分配，成本大头之一）
    let mut keyed: Vec<(usize, String)> = Vec::with_capacity(n);
    for start in (0..n).step_by(CHUNK) {
        let end = (start + CHUNK).min(n);
        keyed.extend(
            entries[start..end]
                .iter()
                .enumerate()
                .map(|(k, e)| (start + k, e.name.to_lowercase())),
        );
        report(end as f64 / n as f64);
    }
    // 阶段 2：分块内部稳定排序（就地，chunk i 占 keyed[i*CHUNK..]）
    for (ci, chunk) in keyed.chunks_mut(CHUNK).enumerate() {
        chunk.sort_by(|x, y| alphanumeric_sort::compare_str(&x.1, &y.1));
        report(((ci + 1) * CHUNK).min(n) as f64 / n as f64);
    }
    // 阶段 3：k 路归并。BinaryHeap 是最大堆，Reverse 一层即按键升序弹出；
    // 键相等按原始下标序（= 整体稳定排序）。节点持有键副本（自持数据，
    // 归并中只读 keyed，无借用冲突）
    struct HeapNode {
        key: String,
        idx: usize,
        chunk: usize,
        pos: usize,
    }
    impl Ord for HeapNode {
        fn cmp(&self, other: &Self) -> Ordering {
            let c = alphanumeric_sort::compare_str(&self.key, &other.key);
            if c == Ordering::Equal {
                self.idx.cmp(&other.idx)
            } else {
                c
            }
        }
    }
    impl PartialOrd for HeapNode {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Eq for HeapNode {}
    impl PartialEq for HeapNode {
        fn eq(&self, other: &Self) -> bool {
            self.cmp(other) == Ordering::Equal
        }
    }
    let mut heap: BinaryHeap<Reverse<HeapNode>> = (0..keyed.len().div_ceil(CHUNK))
        .map(|ci| {
            let (idx, key) = &keyed[ci * CHUNK];
            Reverse(HeapNode {
                key: key.clone(),
                idx: *idx,
                chunk: ci,
                pos: 0,
            })
        })
        .collect();
    let mut order = Vec::with_capacity(n);
    let mut merged = 0usize;
    while let Some(Reverse(mut node)) = heap.pop() {
        order.push(node.idx);
        node.pos += 1;
        merged += 1;
        if merged % CHUNK == 0 {
            report(merged as f64 / n as f64);
        }
        // 同 chunk 的下一元素：chunk 起点闭区间内且不越过 keyed 末尾
        //（尾 chunk 可能不满），否则该 chunk 已耗尽不入堆
        let global = node.chunk * CHUNK + node.pos;
        if node.pos < CHUNK && global < n {
            let (idx, key) = &keyed[global];
            node.key = key.clone();
            node.idx = *idx;
            heap.push(Reverse(node));
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分块排序+归并与整体稳定排序逐位等价：构造跨 chunk 边界（>65536 条）
    /// 且含大量大小写重复键的数据集（相邻两条 lowercase 相同），验证键相同
    /// 时按原始下标序（稳定语义），并有多次进度回报
    #[test]
    fn natural_order_chunked_equivalent_to_full_stable_sort() {
        let n = 70_003usize; // 65536 + 尾 chunk 非满（跨块边界验证）
        let entries: Vec<crate::vfs::VirtualEntry> = (0..n)
            .map(|i| {
                // 相邻两条共享同一 lowercase 键（IMG_5/same.jpg 与 img_5/same.jpg）
                let case = if i % 2 == 0 { "IMG" } else { "img" };
                crate::vfs::VirtualEntry {
                    name: format!("{case}_{}/same.jpg", i / 2),
                    size: i as u64,
                    is_dir: false,
                    encrypted: false,
                    crc: None,
                }
            })
            .collect();
        let progress_calls = std::cell::Cell::new(0usize);
        let got = natural_order_with_progress(&entries, &|_| {
            progress_calls.set(progress_calls.get() + 1)
        });
        // 参照实现：整体稳定排序（旧单次 sort_by 语义）
        let mut keyed: Vec<(usize, String)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (i, e.name.to_lowercase()))
            .collect();
        keyed.sort_by(|x, y| alphanumeric_sort::compare_str(&x.1, &y.1));
        let want: Vec<usize> = keyed.into_iter().map(|(i, _)| i).collect();
        assert_eq!(got, want, "分块排序+归并与整体稳定排序不等价");
        assert!(progress_calls.get() > 1, "跨 chunk 数据集应有多次进度回报");
    }

    /// 评级读取矩阵分流（meta::read_rating，worker 逐条直读）：嵌入式
    /// （jpg 签名 + 属性式 Rating）命中 / sidecar（bmp + .xmp）命中 /
    /// 名单外格式（txt）跳过零读盘 / 未评级与缺失文件 None。
    /// 独立 pid 临时目录（并行测试竞态惯例：禁共享目录 whole-dir 清理互删）
    #[test]
    fn read_rating_矩阵分流与名单外跳过() {
        let dir = std::env::temp_dir().join(format!("archview_read_rating_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 嵌入式：JPEG 签名 + 属性式 Rating（头部文本搜索命中）
        std::fs::write(
            dir.join("a.jpg"),
            b"\xff\xd8\xff\xe0<rdf:Description xmp:Rating=\"4\"/>",
        )
        .unwrap();
        // sidecar 族：bmp 文件本体无 Rating 文本，评级在 .xmp
        std::fs::write(dir.join("p.bmp"), b"BMxxxx").unwrap();
        std::fs::write(dir.join("p.xmp"), "<xmp:Rating>2</xmp:Rating>").unwrap();
        // 名单外：txt 即使含 Rating 文本也不读（rating_kind None 零读盘语义）
        std::fs::write(dir.join("n.txt"), b"<xmp:Rating>5</xmp:Rating>").unwrap();
        // 未评级 jpg：签名合法但无标记 → None（占位回传，计账用）
        std::fs::write(dir.join("u.jpg"), b"\xff\xd8\xff\xe0plain").unwrap();

        assert_eq!(
            crate::meta::read_rating(&dir.join("a.jpg"), "a.jpg"),
            Some(4)
        );
        assert_eq!(
            crate::meta::read_rating(&dir.join("p.bmp"), "p.bmp"),
            Some(2)
        );
        assert_eq!(crate::meta::read_rating(&dir.join("n.txt"), "n.txt"), None);
        assert_eq!(crate::meta::read_rating(&dir.join("u.jpg"), "u.jpg"), None);
        assert_eq!(
            crate::meta::read_rating(&dir.join("missing.jpg"), "missing.jpg"),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 评级探测闭环（调度器直提直读，设计 §19 D8）：enqueue 提交 IoJob 入
    /// Rating 队列 → 调度器读线程读毕 on_done 回投 → poll 合并；
    /// mark-at-enqueue 去重：同一 idx 二次入队不重复提交（结果只回一条）
    #[test]
    fn rating_probe_闭环_提交读毕合并与去重() {
        let mut app = super::App::new_for_test(crate::config::Config::default());
        let dir = std::env::temp_dir().join(format!("archview_probe_new_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.jpg"),
            b"\xff\xd8\xff\xe0<rdf:Description xmp:Rating=\"4\"/>",
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.rating_probe = Some(RatingProbeJob { tx, rx });
        app.rating_probed = vec![false; 1];
        let item = (0usize, dir.join("a.jpg"), "a.jpg".to_string());
        app.rating_probe_enqueue(vec![item.clone()]);
        assert!(app.rating_probed[0], "mark-at-enqueue 置位");
        app.rating_probe_enqueue(vec![item]);
        // 调度器读线程真实读毕回投：只有一条结果（二次入队被去重）
        let got = app
            .rating_probe
            .as_ref()
            .unwrap()
            .rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("探测结果未回投");
        assert_eq!(got, (0, Some(4)));
        assert!(
            app.rating_probe
                .as_ref()
                .unwrap()
                .rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "去重后不应有第二条结果"
        );
        // poll 合并：archive 缺位走 break 分支不崩（合并逻辑 or_insert 本身
        // 由 ratings 表结构保证会话值优先）；筛选在位时 queued 归零走
        // rebuild_view 分支（archive None → 清行缓存，安全）
        app.rating_filter = Some(4);
        app.poll_rating_probe();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 换包会话重置（新取消语义，设计 §19 D8）：cancel(Rating) 清排队任务
    /// + 探测状态清零 + job 摘除；在途读毕迟到结果落空通道（rx 随 job 丢弃，
    /// send 失败静默）不串包。优先级/取消/看门狗归调度器侧单测（iosched.rs）
    #[test]
    fn rating_probe_换包会话重置_取消与清零() {
        use crate::iosched::IoClass;
        let mut app = super::App::new_for_test(crate::config::Config::default());
        let dir = std::env::temp_dir().join(format!("archview_probe_reset_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.jpg"),
            b"\xff\xd8\xff\xe0<rdf:Description xmp:Rating=\"4\"/>",
        )
        .unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        app.rating_probe = Some(RatingProbeJob { tx, rx: _rx });
        app.rating_probed = vec![false; 1];
        app.rating_probe_enqueue(vec![(0usize, dir.join("a.jpg"), "a.jpg".to_string())]);
        app.rating_probe_session_reset();
        assert!(app.rating_probe.is_none(), "job 应随换包摘除");
        assert!(app.rating_probed.is_empty());
        assert!(app.rating_probe_window.is_none());
        assert!(app.rating_probe_intent.is_none());
        assert!(app.rating_image_idx.is_empty());
        assert_eq!(
            app.io.queued(IoClass::Rating),
            0,
            "排队任务应被 cancel 清空（在途任务不在队列计数，读毕落空通道）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
