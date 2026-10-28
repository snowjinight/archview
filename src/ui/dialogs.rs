//! 弹窗：压缩包密码输入（加密头/内容加密两阶段）与「下一个压缩包」选择
//!（本包末项后翻 / N 键主动触发共用）。

use super::*;

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
            // 目录媒体集合模式（ARCHITECTURE.md 4.2）：路径本身是目录，
            // 没有"同目录下一个压缩包"概念
            self.notice = Some("目录集合模式：没有下一个压缩包".into());
            return;
        }
        let list = archives_in_dir(&a.path);
        if list.len() < 2 {
            self.notice = Some("此目录没有其它压缩包".into());
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
}
