//! 弹窗：运行日志（F12，logsink 环形缓冲展示）、压缩包密码输入
//!（加密头/内容加密两阶段）与「下一个压缩包」选择
//!（本包末项后翻 / N 键主动触发共用）。

use super::*;
use crate::encode::cli;

impl App {
    /// 运行日志窗口（F12 开关，nav.rs 接线）：展示 logsink 内存环形缓冲
    /// （最近 600 行，无 ANSI 色）。完整历史落盘 archview.log（exe 同目录），
    /// 「用记事本打开」直达。跟随底部滚动默认开，手动上翻即冻结自动跟随
    pub(super) fn log_window(&mut self, ctx: &egui::Context) {
        if !self.log_open {
            return;
        }
        // 收起态：右缘中部小标签，点击展开——贴边不占内容区，不挡左侧文件列表
        if self.log_collapsed {
            let mut expand = false;
            egui::Area::new(egui::Id::new("log_tab"))
                .anchor(egui::Align2::RIGHT_CENTER, [-4.0, 0.0])
                .show(ctx, |ui| {
                    expand = ui.button("日志 ▶").clicked();
                });
            if expand {
                self.log_collapsed = false;
            }
            return;
        }
        let lines = crate::logsink::snapshot();
        let path = crate::logsink::file_path();
        let mut open = self.log_open;
        let follow = &mut self.log_follow;
        let collapsed = &mut self.log_collapsed;
        // 全宽贴底停靠：fixed_rect 把窗口钉死在视口（同宽、下缘对齐、
        // 高约 38%），不可拖动/缩放，与底部控制栏同一坐标基准，视觉上
        // 像内嵌面板而不是浮动窗
        let scr = ctx
            .input(|i| i.raw.screen_rect)
            .unwrap_or(egui::Rect::NOTHING);
        let h = (scr.height() * 0.38).clamp(220.0, 560.0);
        let rect = egui::Rect::from_min_max(
            egui::pos2(scr.left(), scr.bottom() - h),
            egui::pos2(scr.right(), scr.bottom()),
        );
        egui::Window::new("运行日志 (F12)")
            .open(&mut open)
            .collapsible(false)
            .fixed_rect(rect)
            // Foreground 置顶：egui 交互时把被点面板 move_to_top（同 Middle
            // 层内重排），点击列表/控制栏会把 Middle 层的日志窗口压到面板
            // 之下——"先开 F12 再播放 → 窗口消失、播放中再开 → 正常"的根因
            // （窗口新建时才 move_to_top，之后任何面板交互都会反超）
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(follow, "跟随底部");
                    if ui.button("复制全部").clicked() {
                        ui.ctx().copy_text(lines.join("\n"));
                    }
                    if ui.button("用记事本打开日志文件").clicked() {
                        if let Some(p) = &path {
                            let _ = std::process::Command::new("notepad.exe").arg(p).spawn();
                        }
                    }
                    if ui.button("清屏").clicked() {
                        // 仅清界面视图：文件与 pending 增量不动（日志事实不丢）
                        crate::logsink::clear_view();
                    }
                    // 收起为右缘小标签（egui 自带折叠按钮已禁用——它会把
                    // 窗口缩成左缘标题条挡住文件列表）
                    if ui.button("收起 ▶").clicked() {
                        *collapsed = true;
                    }
                });
                ui.separator();
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(*follow)
                    .show_rows(ui, row_h, lines.len(), |ui, range| {
                        for i in range {
                            ui.add(egui::Label::new(
                                egui::RichText::new(&lines[i]).monospace(),
                            ));
                        }
                    });
            });
        self.log_open = open;
    }

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
            let (entries_idx, alt): (Vec<usize>, Option<Vec<usize>>) = if self.selected.len() >= 2 {
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
                    .filter(|(_, e)| !e.is_dir && cli::preset_index(&self.cfg, &e.name) == Some(pi))
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
                a.path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
            };
            let ve: Vec<VirtualEntry> = entries_idx.iter().map(|&i| a.entries[i].clone()).collect();
            let (fmt, bw) = cli::resolve_encode_format(preset, &ve, &a.vfs)?;
            Ok(EncodeDialog {
                entries: entries_idx,
                alt_entries: alt,
                whole: true,
                preset_match: preset.r#match.clone(),
                fmt,
                out: cli::default_output(&base, &ve[0].name)
                    .display()
                    .to_string(),
                warnings: {
                    let mut w = cli::numbering_gaps(&ve);
                    w.extend(bw);
                    w
                },
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
                    // 真实帧数 = Σ 文件大小/帧字节（resolve_encode_format 已保证
                    // 全体成员对 fmt 帧整除，整除无余数；切换二选一后 entries
                    // 变了但 fmt 不变，同族同分辨率下公式仍精确）
                    let frame_bytes = {
                        let bits =
                            crate::config::raw_pixfmt_bits(&d.fmt.pixfmt).unwrap_or(0) as u64;
                        d.fmt.width as u64 * d.fmt.height as u64 * bits / 8
                    };
                    let frames_of = |idxs: &[usize]| -> usize {
                        let a = self.archive.as_ref();
                        idxs.iter()
                            .map(|&i| {
                                a.map_or(0, |a| (a.entries[i].size / frame_bytes.max(1)) as usize)
                            })
                            .sum()
                    };
                    let whole_frames = frames_of(&d.entries);
                    let alt_frames = d.alt_entries.as_deref().map(frames_of);
                    // 标签按语义归位而非存储位置：swap 后 entries↔alt 互换
                    // （whole=false 时 entries=单文件档），"整个序列"恒指族集、
                    // "仅此文件"恒指单文件档 = if d.whole { entries } else { alt }
                    let (fam_n, fam_f, single_f) = if d.whole {
                        (d.entries.len(), whole_frames, alt_frames.unwrap_or(0))
                    } else {
                        let fam_n = d.alt_entries.as_deref().map_or(0, |a| a.len());
                        (fam_n, alt_frames.unwrap_or(0), whole_frames)
                    };
                    ui.label(format!(
                        "{} 个文件 · 共 {} 帧 · 预设 [{}]（{}×{} {}）",
                        d.entries.len(),
                        whole_frames,
                        d.preset_match,
                        d.fmt.width,
                        d.fmt.height,
                        d.fmt.pixfmt
                    ));
                    // 单选二选一（序列模式；多选精确模式无此项）
                    if d.alt_entries.is_some() {
                        ui.horizontal(|ui| {
                            if ui
                                .radio(
                                    d.whole,
                                    format!("整个序列（{} 个文件 · {} 帧）", fam_n, fam_f),
                                )
                                .clicked()
                                && !d.whole
                            {
                                swap = true;
                            }
                            if ui
                                .radio(!d.whole, format!("仅此文件（{} 帧）", single_f))
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
                d.out = cli::default_output(&base, &ve[0].name)
                    .display()
                    .to_string();
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
                let due = !matches!(last.get(), Some(t) if now.duration_since(t).as_millis() < 100);
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
        // 任务成功启动后清空选中集："这批已提交"语义——确认窗取消路径
        // 不清（用户可能还要调整选择重开窗），只有真正开跑才清
        self.selected.clear();
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

    // ---- 评级延迟批量写落盘（手动同步 + 退出自动保存，2026-11-23 定版）----

    /// 启动评级同步 worker：drain dirty 快照（按路径排序，写序确定）
    /// spawn "ratingsync" 线程逐项写 meta::write_rating。防重入（进行中
    /// 直接返回）；exiting = 退出自动保存（dirty 空时直接放行 Close）。
    /// 删除文件语义在 worker 层：写前 path.exists() 检查，不存在 = 跳过
    ///（不算失败、不碰任何文件含残留 sidecar）——write_rating 保持严格
    pub(super) fn start_rating_sync(&mut self, exiting: bool) {
        if self.rating_sync.is_some() {
            return;
        }
        let mut items: Vec<(std::path::PathBuf, u8)> =
            self.rating_dirty.drain().collect();
        if items.is_empty() {
            if exiting {
                self.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let total = items.len();
        let (tx, rx) = mpsc::channel();
        let ctx = self.egui_ctx.clone();
        std::thread::Builder::new()
            .name("ratingsync".into())
            .spawn(move || {
                for (p, r) in &items {
                    let msg = if !p.exists() {
                        RatingSyncMsg::Skipped
                    } else {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        match crate::meta::write_rating(p, &name, *r) {
                            Ok(()) => RatingSyncMsg::Ok,
                            Err(e) => RatingSyncMsg::Failed(e),
                        }
                    };
                    let _ = tx.send(msg);
                    ctx.request_repaint();
                }
                let _ = tx.send(RatingSyncMsg::Done);
                ctx.request_repaint();
            })
            .ok();
        self.rating_sync = Some(RatingSyncJob {
            rx,
            total,
            done: 0,
            skipped: 0,
            failed: Vec::new(),
            exiting,
        });
        tracing::info!("评级同步启动: {total} 项 exiting={exiting}");
    }

    /// 评级同步任务轮询（update 每帧）：Item 计数；Done 收尾——exiting 时
    /// dirty 又非空（同步期间又评了新项）→ 再启一轮收敛，否则放行退出
    ///（ViewportCommand::Close）；手动同步 → notice/err 汇总。失败不回灌
    /// dirty（用户可再次手动同步重试）
    pub(super) fn poll_rating_sync(&mut self) {
        let mut finished = false;
        if let Some(job) = self.rating_sync.as_mut() {
            loop {
                match job.rx.try_recv() {
                    Ok(RatingSyncMsg::Ok) => job.done += 1,
                    Ok(RatingSyncMsg::Skipped) => {
                        job.done += 1;
                        job.skipped += 1;
                    }
                    Ok(RatingSyncMsg::Failed(e)) => {
                        job.done += 1;
                        job.failed.push(e);
                    }
                    Ok(RatingSyncMsg::Done) | Err(mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        if !finished {
            return;
        }
        let job = self.rating_sync.take().unwrap();
        if job.exiting {
            if self.rating_dirty.is_empty() {
                self.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.start_rating_sync(true);
            }
            return;
        }
        let n = job.total - job.skipped - job.failed.len();
        let mut msg = format!("评级已同步：{n} 项写入");
        if job.skipped > 0 {
            msg += &format!("，{} 项跳过（文件已不存在）", job.skipped);
        }
        if job.failed.is_empty() {
            self.set_notice(msg);
        } else {
            self.set_err(format!(
                "{msg}，{} 项失败：{}",
                job.failed.len(),
                job.failed.join("；")
            ));
        }
    }

    /// 评级同步进度窗（居中 ProgressBar，无取消按钮——退出保存不可中断，
    /// 手动同步项数小无需取消）
    pub(super) fn rating_sync_window(&mut self, ctx: &egui::Context) {
        let Some(job) = self.rating_sync.as_ref() else {
            return;
        };
        let (done, total, skipped, failed) =
            (job.done, job.total, job.skipped, job.failed.len());
        let exiting = job.exiting;
        egui::Window::new(if exiting { "退出自动保存中…" } else { "同步评级到文件" })
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.add(
                    egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                        .show_percentage(),
                );
                ui.weak(format!(
                    "{done}/{total} · 跳过 {skipped}（文件已不存在）· 失败 {failed}"
                ));
                if !exiting {
                    ui.weak("写盘在后台进行，可继续浏览");
                }
            });
    }

    // ---- 选中集删除（仅目录模式，PLAN_image_meta.md §5F；右键菜单 /
    // Ctrl+Delete 两入口。配合星级筛选：筛出不要的 → Ctrl+A 全选 → 删除） ----

    /// 收集当前**选中集**为删除目标并弹确认窗。目录模式 + 选中非空才可达；
    /// 下标排序保证确认窗与执行顺序确定。压缩包模式（无磁盘路径）不可达
    pub(super) fn open_delete_confirm_for_selection(&mut self) {
        let Some(a) = self.archive.as_ref() else {
            return;
        };
        if !a.path.is_dir() {
            return;
        }
        let mut targets: Vec<usize> = self.selected.iter().copied().collect();
        if targets.is_empty() {
            self.set_notice("没有选中可删除的文件");
            return;
        }
        targets.sort_unstable();
        self.delete_confirm = Some(DeleteConfirm { targets });
    }

    /// 「删除选中」确认窗：列出数量/体积，按钮右对齐
    ///（主操作「删除」最右、「取消」居左，见用户 UI 偏好）
    pub(super) fn delete_confirm_window(&mut self, ctx: &egui::Context) {
        let Some(d) = self.delete_confirm.as_ref() else {
            return;
        };
        let (n, total) = {
            let a = self.archive.as_ref().unwrap();
            let total: u64 = d.targets.iter().map(|&i| a.entries[i].size).sum();
            (d.targets.len(), total)
        };
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new("删除选中")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.label(format!(
                    "将把 {} 个文件（共 {:.1} MB）移入系统回收站：",
                    n,
                    total as f64 / 1048576.0
                ));
                ui.weak("回收站中可还原");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // right_to_left：先声明的在右侧——「删除」主操作最右
                        if ui.button("删除").clicked() {
                            confirmed = true;
                        }
                        if ui.button("取消").clicked() {
                            cancelled = true;
                        }
                    });
                });
            });
        if confirmed {
            self.delete_confirmed();
        } else if cancelled {
            self.delete_confirm = None;
        }
    }

    /// 确认删除执行：逐条 trash::delete（回收站，失败逐条报错继续）→
    /// 标记 a.deleted（下标稳定方案，见 ArchiveState.deleted 注释）→
    /// 清评级/选择/锚点、失效图片缓存 → 若正显示被删条目则跳最近的
    /// 存活条目 → 重建视图
    fn delete_confirmed(&mut self) {
        let Some(d) = self.delete_confirm.take() else {
            return;
        };
        let mut errors = 0usize;
        let mut deleted = 0usize;
        // 先记当前播放项（删除后可能需要跳转）
        let was_playing = self.playing;
        let (dir, aid) = {
            let a = self.archive.as_ref().unwrap();
            (a.path.clone(), a.id)
        };
        for idx in &d.targets {
            let name = self.archive.as_ref().unwrap().entries[*idx].name.clone();
            // sidecar 随图删除（评级存储；不存在则静默跳过）
            let sidecar = crate::meta::sidecar_path(&dir.join(&name));
            if sidecar.exists() {
                if let Err(e) = trash::delete(&sidecar) {
                    tracing::warn!("sidecar 删除失败 {}: {e}", sidecar.display());
                }
            }
            match trash::delete(dir.join(&name)) {
                Ok(()) => {
                    deleted += 1;
                    // dirty 集同步移除防幽灵写：文件已删，评级延迟写若残留
                    // 会在同步/退出时把该路径复活成孤儿（worker 跳过不存在的
                    // 文件，但这里清得更干净——语义上"已删除的文件无评级可写"）
                    self.rating_dirty.remove(&dir.join(&name));
                    let key = (aid, *idx);
                    // 像素缓存失效（含 natives：文件已不存在，原图不再有效）
                    self.img.invalidate_full(key);
                    self.img_fallback.remove(&key);
                    let a = self.archive.as_mut().unwrap();
                    a.deleted.insert(*idx);
                    a.ratings.remove(idx);
                }
                Err(e) => {
                    errors += 1;
                    tracing::warn!("删除失败 {}: {e}", name);
                }
            }
        }
        // 多选/锚点清掉被删项（残留下标会指向不可达条目）；块内结束 a 借用
        let playing_gone = {
            let a = self.archive.as_mut().unwrap();
            self.selected.retain(|i| !a.deleted.contains(i));
            self.select_anchor = self.select_anchor.filter(|i| !a.deleted.contains(i));
            self.last_focus = self.last_focus.filter(|i| !a.deleted.contains(i));
            was_playing.is_some_and(|p| a.deleted.contains(&p))
        };
        self.rebuild_view();
        // 正在看的图被删：跳筛选后最近的一个存活条目（向后优先，无则向前）
        if playing_gone {
            let p = was_playing.unwrap();
            let next = self.archive.as_ref().unwrap().view.iter().find(|&&i| i > p)
                .or_else(|| {
                    self.archive.as_ref().unwrap().view.iter().rev().find(|&&i| i < p)
                })
                .copied();
            match next {
                Some(i) => self.play(i),
                None => {
                    self.playing = None;
                    if let Some(pl) = &self.player {
                        pl.stop();
                    }
                }
            }
        }
        let msg = format!("已删除 {deleted} 个文件（进回收站）");
        if errors > 0 {
            self.set_err(format!("{msg}，{errors} 个失败（详见日志）"));
        } else {
            self.set_notice(msg);
        }
        tracing::info!("星级删除：{deleted} 成功 / {errors} 失败");
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
