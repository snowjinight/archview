//! 打开链路：文件/目录对话框、拖放入口、密码分支、后台打开任务轮询与视图重建。

use super::*;

impl App {
    // ---- 打开压缩包 ----

    pub(super) fn open_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Archives", &["zip", "7z", "rar"])
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
                } => {
                    let to_play =
                        self.apply_open_done(job.path, job.retry_play, vfs, id, entries, pw);
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
    ) -> Option<usize> {
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
            })),
        });
        // 新包旧选择无意义：清多选与范围锚点（entry 下标对新包是错位引用）
        self.selected.clear();
        self.select_anchor = None;
        self.rebuild_view();
        // 单媒体文件入口：目标文件所在目录打开完成（仅目录模式消费——
        // 压缩包路径不吃焦点，防"焦点未消费又被压缩包打开覆盖"的竞态串播）。
        // 按目标扩展名预切标签页：混合内容目录下不预切的话目标在另一
        // 标签页里，跳转落空；已在该页则不再多算一次行缓存
        let focus = if path.is_dir() { self.pending_focus.take() } else { None };
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

    /// 重新计算过滤视图：搜索框高级语法匹配（search_engine：空格 AND、| OR、
    /// ! 排除、"精确短语"、* ? 通配、<分组>，语法说明见根目录 serach.md）
    /// + 只显示媒体文件（视频/图片按 config 分类），
    /// 按文件名自然序排序（数字段按数值比较：1/2/10，而非字典序的 1/10/2；
    /// 保证序号/上一个下一个顺序稳定，与归档内部顺序无关）。
    /// 同时重建列表行缓存（虚拟化渲染的显示数据）：只在打开/搜索/切标签时执行，
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
        let mut classified: Vec<(usize, bool)> = a
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if e.is_dir || !query.matches(&e.name) {
                    return None;
                }
                let is_video = self.cfg.is_video_ext(&e.name);
                (is_video || self.cfg.is_image_ext(&e.name)).then_some((i, is_video))
            })
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
            // 键与 (idx, 分类) 绑定排序再取回：排序中不会错位
            let mut keyed: Vec<(usize, bool, String)> = classified
                .iter()
                .map(|&(i, v)| (i, v, a.entries[i].name.to_lowercase()))
                .collect();
            keyed.sort_by(|x, y| alphanumeric_sort::compare_str(&x.2, &y.2));
            classified = keyed.into_iter().map(|(i, v, _)| (i, v)).collect();
        }
        let (view, kinds): (Vec<usize>, Vec<bool>) = classified.into_iter().unzip();
        a.view = view;

        // 自动切标签页：当前标签页为空而另一类非空时跳过去。
        // 纯图片/纯视频的包打开时不再停在空列表上让用户误以为没打开
        // （搜索过滤后的空结果不切——那属于"没匹配"而非"没这类文件"，
        //   需要搜索框为空才切，避免打断用户在另一标签页里搜东西；
        //   注意不能用 query.is_empty()：输入 "!" 等无法成词的内容也会解析为空树）
        if self.search.trim().is_empty() {
            let (n_video, n_image) = kinds
                .iter()
                .fold((0usize, 0usize), |(v, m), &is_video| {
                    if is_video { (v + 1, m) } else { (v, m + 1) }
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
