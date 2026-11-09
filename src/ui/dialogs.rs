//! 弹窗：压缩包密码输入（加密头/内容加密两阶段）与「下一个压缩包」选择
//!（本包末项后翻 / N 键主动触发共用）。

use super::*;
use crate::encode::cli;

impl App {
    /// 密码输入弹窗（加密头=打开阶段 / 加密条目=播放阶段）。窗口视口居中
    pub(super) fn password_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = self.pw_prompt.as_mut() else {
            return;
        };
        // anchor 视口居中：egui 逻辑坐标下 125%/150% 缩放均保持居中
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

    /// 「打开下一个压缩包」选择弹窗（本包末项后翻 / N 键主动触发共用）。
    /// 列出同目录全部压缩包（分卷族合并为首卷），默认选中循环意义上的
    /// 下一个（末包绕回第一个），点选任意包可直接打开；
    /// 三态区分（加粗=当前 / 弱灰=本会话看过 / 琥珀=未看）帮助绕圈时分辨。
    pub(super) fn next_archive_dialog(&mut self, ctx: &egui::Context) {
        // 先取当前包路径（“当前”标记）与已看集合（弱灰标记），避免与 prompt 可变借用冲突
        let cur_name = self
            .archive
            .as_ref()
            .and_then(|a| a.path.file_name())
            .map(|n| n.to_string_lossy().to_lowercase());
        let seen = self.seen_archives.clone();
        let Some(prompt) = self.next_archive_prompt.as_mut() else {
            return;
        };
        let mut open = false;
        let mut cancelled = false;
        egui::Window::new("打开下一个压缩包？")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                // 三态区分：未看过=琥珀（暖色=待看新内容，与选中蓝互补不冲突）/
                // 已看过=弱灰（看过即淡出）/ 当前=正常色加粗（不着色——选中行
                // 本身是蓝色高亮底，再用蓝色标“当前”会与选中态混淆）
                let unseen_c = if ui.visuals().dark_mode {
                    egui::Color32::from_rgb(232, 178, 72)
                } else {
                    egui::Color32::from_rgb(168, 118, 20)
                };
                // 文案区分触发来源：末项后翻带“已到最后一项”；
                // N 键主动触发（还没看完）只列选择
                ui.label(if prompt.at_end {
                    "已到本压缩包的最后一项，选择要打开的压缩包："
                } else {
                    "选择要打开的压缩包："
                });
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
                            // 当前=加粗（不着色）/ 未看过=琥珀 / 已看过=弱灰
                            let text = if is_cur {
                                egui::RichText::new(label).strong()
                            } else if !seen.contains(path) {
                                egui::RichText::new(label).color(unseen_c)
                            } else {
                                egui::RichText::new(label).weak()
                            };
                            if ui.selectable_label(prompt.sel == i, text).clicked() {
                                prompt.sel = i;
                            }
                        }
                    });
                // 颜色图例（小号，颜色/字重即含义）
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("当前").small().strong());
                    ui.label(egui::RichText::new("已看过").small().weak());
                    ui.label(egui::RichText::new("未看过").small().color(unseen_c));
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

    /// 弹出「打开下一个压缩包」选择窗（N 键 / 本包末项后继续后翻共用）。
    /// 列出同目录全部压缩包（分卷族合并为首卷），默认选中循环意义上的
    /// 下一个（末包绕回第一个）；目录集合模式 / 无其它包时给出一次性提示。
    /// at_end=true（末项后翻）：弹窗文案带“已到最后一项”；false（N 键）则不带。
    pub(super) fn ask_next_archive(&mut self, at_end: bool) {
        if !self.cfg.ui.ask_next_archive || self.next_archive_prompt.is_some() {
            return;
        }
        let Some(a) = &self.archive else { return };
        if a.path.is_dir() {
            // 目录媒体集合模式（doc/架构设计/04_VFS层.md §4.2）：路径本身是目录，
            // 没有"同目录下一个压缩包"概念
            self.set_notice("目录集合模式：没有下一个压缩包");
            return;
        }
        let list = archives_in_dir(&a.path);
        if list.len() < 2 {
            self.set_notice("此目录没有其它压缩包");
            return;
        }
        // 默认选中循环意义上的下一个（末包绕回第一个）
        let sel = next_archive_in_dir(&a.path)
            .and_then(|p| list.iter().position(|q| *q == p))
            .unwrap_or(0);
        self.next_archive_prompt = Some(NextArchivePrompt {
            archives: list,
            sel,
            at_end,
        });
    }

    // ============ 「编码为视频…」确认窗 + 后台任务（§15.5 UI 执行） ============

    /// 弹出「编码为视频…」确认窗（右键菜单入口）。ctx_row = 右键行
    /// （序列模式的目标行）；selected ≥2 时精确模式优先（所见即所选，
    /// 忽略右键行）。校验失败（未命中预设/混族）以红字报错，不开窗
    pub(super) fn open_encode_dialog(&mut self, ctx_row: Option<usize>) {
        if self.encode_job.is_some() {
            self.set_notice("已有编码任务进行中（同时仅一个）");
            return;
        }
        // 组装期只读借 archive/cfg（错误文案先收集，借束后统一 set_err）
        let assembled: Result<EncodeDialog, String> = (|| {
            let a = self.archive.as_ref().ok_or("未打开压缩包")?;
            // ---- 模式与条目集：多选 ≥2 = 精确（自然序）；否则以目标行定位整族 ----
            let (entries_idx, alt): (Vec<usize>, Option<Vec<usize>>) =
                if self.selected.len() >= 2 {
                    let mut v: Vec<usize> = self.selected.iter().copied().collect();
                    v.sort_by(|&x, &y| {
                        alphanumeric_sort::compare_str(&a.entries[x].name, &a.entries[y].name)
                    });
                    (v, None)
                } else {
                    let row = ctx_row.or_else(|| self.selected.iter().next().copied());
                    let row = row.ok_or("没有可编码的目标行")?;
                    let name = &a.entries[row].name;
                    let pi = cli::preset_index(&self.cfg, name).ok_or_else(|| {
                        format!("文件 [{name}] 未命中任何 [[raw.demux]] 预设（config.toml）")
                    })?;
                    // UI 版带下标族收集：全包条目按同一预设分组（组内 name 自然序）。
                    // 不走 cli::families——它返回条目本体，name→下标回查是 O(n²)
                    let mut fam: Vec<usize> = a
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| {
                            !e.is_dir && cli::preset_index(&self.cfg, &e.name) == Some(pi)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    fam.sort_by(|&x, &y| {
                        alphanumeric_sort::compare_str(&a.entries[x].name, &a.entries[y].name)
                    });
                    (fam, Some(vec![row]))
                };
            if entries_idx.is_empty() {
                return Err("没有可编码的文件".into());
            }
            // ---- 族一致性校验（CLI 同款文案；序列模式恒过——整族同预设）----
            let mut fam_id: Option<usize> = None;
            for &i in &entries_idx {
                let name = &a.entries[i].name;
                match cli::preset_index(&self.cfg, name) {
                    None => {
                        return Err(format!(
                            "文件 [{name}] 未命中任何 [[raw.demux]] 预设（config.toml）"
                        ))
                    }
                    Some(p) => match fam_id {
                        None => fam_id = Some(p),
                        Some(prev) if prev == p => {}
                        Some(prev) => {
                            let x = &self.cfg.raw.demux[prev].r#match;
                            let y = &self.cfg.raw.demux[p].r#match;
                            return Err(format!(
                                "选中的文件命中多个不同预设（{x} 与 {y}），\
                                 不是同一序列；请只选同一族的文件"
                            ));
                        }
                    },
                }
            }
            let preset = &self.cfg.raw.demux[fam_id.unwrap()];
            // 输出基目录：压缩包=所在目录；目录集合模式=目录本身（与 CLI 同口径）
            let base = if a.path.is_dir() {
                a.path.clone()
            } else {
                a.path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default()
            };
            let ve: Vec<VirtualEntry> =
                entries_idx.iter().map(|&i| a.entries[i].clone()).collect();
            Ok(EncodeDialog {
                entries: entries_idx,
                alt_entries: alt,
                whole: true,
                preset_match: preset.r#match.clone(),
                fmt: cli::encode_format(preset),
                out: cli::default_output(&base, &ve[0].name)
                    .display()
                    .to_string(),
                warnings: cli::numbering_gaps(&ve),
            })
        })();
        match assembled {
            Ok(d) => self.encode_dialog = Some(d),
            Err(e) => self.set_err(e),
        }
    }

    /// 「编码为视频…」双形态窗（§15.5）：任务在跑 = 进度窗（进度条+取消）；
    /// 否则 = 确认窗（概要 / 二选一 / fps / 输出路径 / 警告 / 开始）
    pub(super) fn encode_dialog_window(&mut self, ctx: &egui::Context) {
        // ---- 形态一：进度窗（后台任务在跑） ----
        if let Some(job) = self.encode_job.as_ref() {
            let cancel = job.cancel.clone();
            let progress = job.progress;
            let out_disp = job.out.display().to_string();
            let secs = job.started.elapsed().as_secs_f64();
            let mut cancelled = false;
            egui::Window::new("编码中…")
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.set_min_width(360.0);
                    ui.label(format!("输出：{out_disp}"));
                    ui.add_space(6.0);
                    ui.add(
                        egui::ProgressBar::new((progress / 100.0).clamp(0.0, 1.0))
                            .show_percentage(),
                    );
                    ui.weak(format!("已运行 {secs:.0} s · 编码器独立于播放，可继续浏览"));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("取消编码").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                });
            if cancelled {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        // ---- 形态二：确认窗 ----
        let mut start = false;
        let mut cancelled = false;
        let mut swap = false;
        if let Some(d) = self.encode_dialog.as_mut() {
            let out_trim = d.out.trim().to_string();
            let out_exists = !out_trim.is_empty() && std::path::Path::new(&out_trim).exists();
            egui::Window::new("编码为视频")
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.set_min_width(400.0);
                    ui.label(format!(
                        "{} 个文件 · 预设 [{}]（{}×{} {}）",
                        d.entries.len(),
                        d.preset_match,
                        d.fmt.width,
                        d.fmt.height,
                        d.fmt.pixfmt
                    ));
                    // 单选二选一（序列模式；多选精确模式无此项）
                    if let Some(alt) = &d.alt_entries {
                        ui.horizontal(|ui| {
                            if ui
                                .radio(d.whole, format!("整个序列（{} 帧）", d.entries.len()))
                                .clicked()
                                && !d.whole
                            {
                                swap = true;
                            }
                            if ui.radio(!d.whole, format!("仅此文件（{} 帧）", alt.len()))
                                .clicked()
                                && d.whole
                            {
                                swap = true;
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label("帧率：");
                        ui.add(
                            egui::DragValue::new(&mut d.fmt.fps)
                                .range(1.0..=240.0)
                                .speed(0.5)
                                .suffix(" fps"),
                        );
                        ui.weak("来自预设，可改");
                    });
                    ui.horizontal(|ui| {
                        ui.label("输出：");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.out)
                                .desired_width((ui.available_width() - 40.0).max(60.0)),
                        );
                    });
                    if out_exists {
                        ui.colored_label(
                            egui::Color32::RED,
                            "输出文件已存在——拒绝覆盖，请修改路径",
                        );
                    }
                    for w in &d.warnings {
                        let warn_c = if ui.visuals().dark_mode {
                            egui::Color32::from_rgb(224, 172, 76)
                        } else {
                            egui::Color32::from_rgb(146, 102, 12)
                        };
                        ui.colored_label(warn_c, format!("警告：{w}"));
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    !out_exists && !out_trim.is_empty(),
                                    egui::Button::new("开始编码"),
                                )
                                .clicked()
                            {
                                start = true;
                            }
                            if ui.button("取消").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                });
        }
        if swap {
            self.encode_dialog_swap();
        }
        if start {
            self.start_encode();
        } else if cancelled {
            self.encode_dialog = None;
        }
    }

    /// 确认窗二选一切换：交换 entries ↔ alt_entries（whole 翻转），重算
    /// 默认输出与警告（首帧变了，断点集合随之变）
    fn encode_dialog_swap(&mut self) {
        let Some(mut d) = self.encode_dialog.take() else {
            return;
        };
        if let Some(a) = self.archive.as_ref() {
            if let Some(alt) = d.alt_entries.take() {
                let prev = std::mem::replace(&mut d.entries, alt);
                d.alt_entries = Some(prev);
                d.whole = !d.whole;
                let base = if a.path.is_dir() {
                    a.path.clone()
                } else {
                    a.path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
                };
                let ve: Vec<VirtualEntry> =
                    d.entries.iter().map(|&i| a.entries[i].clone()).collect();
                d.out =
                    cli::default_output(&base, &ve[0].name).display().to_string();
                d.warnings = cli::numbering_gaps(&ve);
            }
        }
        self.encode_dialog = Some(d);
    }

    /// 从确认窗启动后台编码（§15.5：线程跑引擎 + mpsc 回报；同时仅一个
    /// 任务，编码 mpv 实例独立——播放不受影响）。dialog 随之关闭，进度
    /// 由 encode_dialog_window 的进度窗形态接管
    fn start_encode(&mut self) {
        let Some(d) = self.encode_dialog.take() else {
            return;
        };
        let Some(a) = self.archive.as_ref() else {
            return;
        };
        if d.out.trim().is_empty() {
            self.set_err("输出路径为空");
            return;
        }
        let out = PathBuf::from(d.out.trim());
        if out.exists() {
            self.set_err("输出文件已存在，拒绝覆盖");
            return;
        }
        let entries: Vec<VirtualEntry> = d.entries.iter().map(|&i| a.entries[i].clone()).collect();
        let vfs = a.vfs.clone();
        let fmt = d.fmt;
        let dll = self.cfg.dll_path();
        let opts = cli::encode_options(&self.cfg.encode);
        tracing::info!(
            "UI 编码任务启动：{} 个文件（{}x{} {} @ {}fps）→ {}",
            entries.len(),
            fmt.width,
            fmt.height,
            fmt.pixfmt,
            fmt.fps,
            out.display()
        );
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_t = cancel.clone();
        let egui_ctx = self.egui_ctx.clone();
        let out_t = out.clone();
        std::thread::spawn(move || {
            // 进度节流：引擎每帧回调（全速转码下极密），≥100ms 才发一次；
            // Cell 单线程内部可变性（回调是 &dyn Fn，不能 FnMut）
            let last = std::cell::Cell::new(None::<std::time::Instant>);
            let progress = |p: f64| {
                let now = std::time::Instant::now();
                let due =
                    !matches!(last.get(), Some(t) if now.duration_since(t).as_millis() < 100);
                if due {
                    last.set(Some(now));
                    let _ = tx.send(EncodeMsg::Progress(p as f32));
                    egui_ctx.request_repaint();
                }
            };
            let r = crate::encode::encode_raw_sequence(
                &dll, &vfs, &entries, &fmt, &out_t, &opts, &cancel_t, &progress,
            );
            let _ = tx.send(match r {
                Ok(s) => EncodeMsg::Done(s),
                Err(e) => EncodeMsg::Failed(e),
            });
            egui_ctx.request_repaint();
        });
        self.encode_job = Some(EncodeJob {
            cancel,
            rx,
            out,
            started: std::time::Instant::now(),
            progress: 0.0,
        });
    }

    /// 后台编码任务轮询（update 每帧）：Progress 刷新进度值；终态三分支
    /// ——Done 绿条 / "已取消" 绿条（用户主动，非错误）/ 其它失败红字。
    /// 任务收尾即清槽（下个任务可启动）
    pub(super) fn poll_encode(&mut self) {
        let mut terminal: Option<Result<crate::encode::EncodeStats, String>> = None;
        if let Some(job) = self.encode_job.as_mut() {
            loop {
                match job.rx.try_recv() {
                    Ok(EncodeMsg::Progress(p)) => job.progress = p,
                    Ok(EncodeMsg::Done(s)) => {
                        terminal = Some(Ok(s));
                        break;
                    }
                    Ok(EncodeMsg::Failed(e)) => {
                        terminal = Some(Err(e));
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        terminal = Some(Err("编码线程已退出".into()));
                        break;
                    }
                }
            }
        }
        if let Some(res) = terminal {
            let job = self.encode_job.take().unwrap();
            let secs = job.started.elapsed().as_secs_f64();
            match res {
                Ok(s) => {
                    self.set_notice(format!(
                        "编码完成：{} 帧 → {}（{:.1}s，点击打开所在目录）",
                        s.frames,
                        job.out.display(),
                        secs
                    ));
                    // 绿条伴随动作：整条可点 → 打开输出所在目录（§15.5）
                    self.encode_done_dir = job.out.parent().map(|p| p.to_path_buf());
                }
                Err(e) if e == "已取消" => self.set_notice("已取消编码"),
                Err(e) => self.set_err(e),
            }
        }
    }
}

/// 「关于」下拉面板文案（顶栏「关于」按钮，panels.rs Popup 渲染）。
/// 独立纯函数：版本行随 Cargo.toml 自动同步（CARGO_PKG_VERSION）、作者/
/// 版权行与仓库 LICENSE 保持一致——单测钉住防手写漂移。
pub(super) fn about_lines() -> Vec<String> {
    vec![
        "Archive Media Player（archview）".into(),
        format!("版本 {}", env!("CARGO_PKG_VERSION")),
        "作者 snowjinight".into(),
        "Copyright (c) 2026 snowjinight · MIT License".into(),
    ]
}
