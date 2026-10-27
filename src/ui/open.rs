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
    /// 与拖放/命令行参数同一条规则，ARCHITECTURE.md 4.2）。
    /// 空集合提示在打开完成时统一给出（poll_open 的 Done 分支）
    pub(super) fn open_dir_dialog(&mut self) {
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        self.open_path(dir);
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
        // 目录集合的媒体扩展名过滤集（视频+图片，配置驱动；压缩包等其它文件天然被排除）
        let media_exts: Vec<String> = self
            .cfg
            .media
            .video_extensions
            .iter()
            .chain(self.cfg.media.image_extensions.iter())
            .cloned()
            .collect();
        let spawned = std::thread::Builder::new()
            .name("open_archive".into())
            .spawn(move || {
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
                // 目录模式：目录即媒体集合（DirVFS 递归收集图片/视频，跳过压缩包，
                // ARCHITECTURE.md 4.2）——无加密概念，不走密码探测链；文件模式照旧
                let opened = if path_t.is_dir() {
                    DirVFS::open(&path_t, &media_exts)
                        .map(|v| (Box::new(v) as Box<dyn ArchiveVFS>, None))
                } else {
                    // 打开阶段即探测并锁定密码（ARCHITECTURE.md 12.5）：
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
        self.rebuild_view();
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
        // 空集合提示（目录里没有媒体 / 压缩包内没有媒体文件）——
        // 统一在此给出，各打开入口不必各自判断
        if self.archive.as_ref().map_or(false, |a| a.view.is_empty()) {
            self.notice = Some("没有可播放的视频/图片".into());
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
        self.error = None;
        self.pw_prompt = None;
        // 密码重试场景：恢复播放提交密码时想看的条目（返回调用方执行）
        retry_play.map(|(entry_idx, _)| entry_idx)
    }

    /// 重新计算过滤视图：搜索框子串匹配 + 只显示媒体文件（视频/图片按 config 分类），
    /// 按文件名自然序排序（数字段按数值比较：1/2/10，而非字典序的 1/10/2；
    /// 保证序号/上一个下一个顺序稳定，与归档内部顺序无关）。
    /// 同时重建列表行缓存（虚拟化渲染的显示数据）：只在打开/搜索/切标签时执行，
    /// 不再每帧重算——万级条目时每帧 clone/format 是 UI 卡顿的主因
    pub(super) fn rebuild_view(&mut self) {
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
}
