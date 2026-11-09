//! 内嵌 config.toml 编辑器（简单版）：纯文本编辑 + '#' 注释灰色高亮，
//! Ctrl+C/V/A/Z 与主流编辑器一致（egui 原生），另支持 Ctrl+S 保存与
//! Ctrl+/ 多行注释切换（加注/解除由选中行整体状态决定，VSCode 惯例）。
//! 「保存并生效」走与启动完全相同的解析路径校验（toml::from_str::<Config>，
//! 语法与类型错都拦在写盘前），通过才写盘并复用 reload_config 热生效。
//!
//! 高亮实现：自定义 layouter 逐行找注释起点（跳过双引号字符串内的 '#'），
//! 注释段用弱色（weak_text_color，随明暗主题）。只做注释一档——键/字符串/
//! 数字多色需完整词法器，简单版不做。

use super::App;
use crate::config::Config;

/// 编辑器状态（App.config_editor 持有；None = 窗口关闭）
pub(super) struct ConfigEditor {
    /// 编辑中的全文（与磁盘独立；「放弃更改」可还原）
    text: String,
    /// 上次「保存并生效」的失败原因（含行列号；None = 无错误）
    error: Option<String>,
}

impl ConfigEditor {
    /// 从磁盘载入当前生效的 config.toml（缺文件先重建默认模板，与
    /// 「外部程序打开」一致）；读取失败以错误占位（空文本）
    pub(super) fn load() -> Self {
        let path = Config::config_path();
        if !path.exists() {
            let _ = Config::load_or_create();
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => Self { text, error: None },
            Err(e) => Self {
                text: String::new(),
                error: Some(format!("读取 {} 失败: {e}", path.display())),
            },
        }
    }
}

/// 行内注释起点（'#' 字节下标）：跟踪双引号字符串与 '\' 转义，字符串内
/// '#' 不算注释。单引号字面串不跟踪——默认模板字符串一律双引号，误判
/// 仅影响染色不影响保存校验。纯函数，单测覆盖
fn comment_start(line: &str) -> Option<usize> {
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
        } else if ch == '#' {
            return Some(i);
        }
    }
    None
}

/// 切换选中行（[first, last]，0 起含端点）的注释状态，VSCode 惯例：
/// - 行首缩进后紧跟 '#' 视为已注释；
/// - 选中行全部已注释（或空行，且至少一行非空）→ 统一解除；否则统一加注；
/// - 加注：在缩进后插 "# "；解除：去掉 '#' 及其后至多一个空格。
///   返回 (新文本, 受影响行块在新文本中的 char 起点与终点偏移)——
///   偏移供调用方把光标选区恢复为覆盖整个行块（可连按反复切换）。
///   纯函数，单测覆盖
fn toggle_comment_lines(text: &str, first: usize, last: usize) -> (String, usize, usize) {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    let first = first.min(n - 1);
    let last = last.clamp(first, n - 1);

    let commented = |l: &str| l.trim_start().starts_with('#');
    let blank = |l: &str| l.trim().is_empty();
    let sel = &lines[first..=last];
    let uncomment = sel.iter().any(|l| !blank(l))
        && sel.iter().all(|l| blank(l) || commented(l));

    let mut out = String::with_capacity(text.len() + sel.len() * 2);
    let mut block_start = 0usize;
    let mut block_end = 0usize;
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if i == first {
            block_start = out.chars().count();
        }
        if (first..=last).contains(&i) {
            // 前导空白必为 ASCII，byte 切片安全
            let indent = l.len() - l.trim_start_matches([' ', '\t']).len();
            if uncomment {
                let rest = l[indent..].strip_prefix('#').unwrap_or(&l[indent..]);
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                out.push_str(&l[..indent]);
                out.push_str(rest);
            } else {
                out.push_str(&l[..indent]);
                out.push_str("# ");
                out.push_str(&l[indent..]);
            }
        } else {
            out.push_str(l);
        }
        if i == last {
            block_end = out.chars().count();
        }
    }
    (out, block_start, block_end)
}

/// 注释灰色高亮的排版：按注释起点把每行切成两段，注释段用弱色。
/// wrap_width 由 TextEdit 传入（换行行为与默认一致）
fn highlight_galley(
    text: &str,
    ui: &egui::Ui,
    wrap_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let plain = egui::TextFormat::simple(font_id.clone(), ui.visuals().text_color());
    let comment = egui::TextFormat::simple(font_id, ui.visuals().weak_text_color());
    // split_inclusive 保留 '\n'：排版文本必须与编辑缓冲逐字节一致
    // （TextEdit 的光标定位/命中测试都基于 galley 文本）
    for line in text.split_inclusive('\n') {
        match comment_start(line) {
            Some(i) => {
                job.append(&line[..i], 0.0, plain.clone());
                job.append(&line[i..], 0.0, comment.clone());
            }
            None => job.append(line, 0.0, plain.clone()),
        }
    }
    job.wrap.max_width = wrap_width;
    ui.ctx().fonts_mut(|f| f.layout_job(job))
}

impl App {
    /// 内嵌 config.toml 编辑器窗口（「配置 → 编辑 config.toml」打开）。
    /// 保存 = 校验 → 写盘 → 关窗 → reload_config 热生效；
    /// 校验/写盘失败窗口保留并显示错误（编辑内容不丢）
    pub(super) fn config_editor_window(&mut self, ctx: &egui::Context) {
        let Some(ed) = self.config_editor.as_mut() else { return };
        let mut open = true;
        let mut save = false;
        let mut reset = false;
        egui::Window::new("config.toml 编辑器")
            .open(&mut open)
            .default_size([820.0, 680.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("保存并生效").clicked() {
                        save = true;
                    }
                    if ui.button("放弃更改").clicked() {
                        reset = true;
                    }
                    ui.weak("Ctrl+C/V/A/Z 同主流编辑器；# 注释灰色；Ctrl+/ 切换注释；Ctrl+S 保存");
                });
                ui.weak(Config::config_path().display().to_string());
                if let Some(err) = &ed.error {
                    ui.colored_label(ui.visuals().error_fg_color, err);
                }
                // Ctrl+S：编辑器肌肉记忆，防"以为存了"（写盘/校验失败会保留窗口提示）。
                // 修饰键取 Key 事件内联字段（原子快照，免疫失焦清零——doc/架构落地开发/11_修饰键两条数据路径.md §11）
                if ui.input(|i| {
                    i.events.iter().any(|e| {
                        matches!(
                            e,
                            egui::Event::Key {
                                key: egui::Key::S,
                                pressed: true,
                                repeat: false,
                                modifiers,
                                ..
                            } if modifiers.ctrl
                        )
                    })
                }) {
                    save = true;
                }
                // 行高按内容行数铺满 + 外层 ScrollArea 滚动（egui 光标移动
                // 会自动 scroll_to_rect 联动外层滚动区，见 text_edit builder.rs）
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut layouter = |ui: &egui::Ui,
                                            buf: &dyn egui::TextBuffer,
                                            wrap_width: f32| {
                            highlight_galley(buf.as_str(), ui, wrap_width)
                        };
                        let rows = ed.text.lines().count().max(20) + 1;
                        let out = egui::TextEdit::multiline(&mut ed.text)
                            .desired_width(f32::INFINITY)
                            .desired_rows(rows)
                            .layouter(&mut layouter)
                            .show(ui);

                        // Ctrl+/：选中行注释切换（无选区 = 光标所在行）。
                        // TextEdit 无此内置命令不消费按键；cursor_range 仅在
                        // 编辑器持有焦点时为 Some（恰好是语义要求）。
                        // 修饰键取 Key 事件内联字段（doc/架构落地开发/11_修饰键两条数据路径.md §11）
                        if let Some(cr) = out.cursor_range {
                            if ui.input(|i| {
                                i.events.iter().any(|e| {
                                    matches!(
                                        e,
                                        egui::Event::Key {
                                            key: egui::Key::Slash,
                                            pressed: true,
                                            repeat: false,
                                            modifiers,
                                            ..
                                        } if modifiers.ctrl && !modifiers.shift
                                    )
                                })
                            }) {
                                let a: usize = cr.primary.index.into();
                                let b: usize = cr.secondary.index.into();
                                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                                let line_of = |off: usize| {
                                    ed.text.chars().take(off).filter(|c| *c == '\n').count()
                                };
                                // 选区末端恰停在行首且非空选区 → 该行不参与
                                // （选区覆盖到上一行行尾，VSCode 惯例）
                                let last = if hi > lo
                                    && ed.text.chars().nth(hi - 1) == Some('\n')
                                {
                                    line_of(hi) - 1
                                } else {
                                    line_of(hi)
                                };
                                let (new_text, bs, be) =
                                    toggle_comment_lines(&ed.text, line_of(lo), last);
                                ed.text = new_text;
                                // 选区恢复为覆盖整个行块（连按切换不丢范围）
                                if let Some(mut state) =
                                    egui::text_edit::TextEditState::load(
                                        ui.ctx(),
                                        out.response.id,
                                    )
                                {
                                    state.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::two(
                                            egui::text::CCursor::new(bs),
                                            egui::text::CCursor::new(be),
                                        ),
                                    ));
                                    state.store(ui.ctx(), out.response.id);
                                }
                            }
                        }
                    });
            });
        if reset {
            *ed = ConfigEditor::load();
        }
        if !open {
            self.config_editor = None;
        } else if save {
            self.save_config_editor();
        }
    }

    /// 编辑器「保存并生效」：与启动完全同路径校验（toml::from_str::<Config>，
    /// 语法与类型错都拦在写盘前），通过 → 写盘 → 关窗 → 复用 reload_config
    /// （热生效 + notice 反馈）。写盘后 reload 重读的是同一文本，解析必成；
    /// 用户删掉的键由 backfill 自动补全（下次打开编辑器即见）
    fn save_config_editor(&mut self) {
        let Some(ed) = self.config_editor.as_ref() else { return };
        let text = ed.text.clone();
        if let Err(e) = toml::from_str::<Config>(&text) {
            let msg = format!(
                "TOML 语法/类型错误（未保存）: {e}\n\
                 （字符串必须双引号；路径写正斜杠 D:/dir 或双反斜杠 D:\\\\dir）"
            );
            if let Some(ed) = self.config_editor.as_mut() {
                ed.error = Some(msg);
            }
            return;
        }
        let path = Config::config_path();
        match std::fs::write(&path, &text) {
            Ok(()) => {
                self.config_editor = None;
                self.reload_config();
            }
            Err(e) => {
                let msg = format!("写入 {} 失败: {e}", path.display());
                if let Some(ed) = self.config_editor.as_mut() {
                    ed.error = Some(msg);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{comment_start, toggle_comment_lines};

    #[test]
    fn 注释起点识别() {
        assert_eq!(comment_start("# 全行注释"), Some(0));
        assert_eq!(comment_start("a = 1 # 尾注"), Some(6));
        assert_eq!(comment_start("a = 1"), None);
        assert_eq!(comment_start(""), None);
    }

    #[test]
    fn 字符串内的井号不是注释() {
        assert_eq!(comment_start("p = \"a#b\""), None);
        // 转义引号后仍在字符串内
        assert_eq!(comment_start("p = \"a\\\"#b\""), None);
        // 字符串闭合后的 # 才是注释
        assert_eq!(comment_start("p = \"x\" # c"), Some(8));
    }

    #[test]
    fn 注释切换_加注与缩进() {
        let text = "a = 1\n\tb = 2\nc";
        // 选中 0..=1 行：加注（缩进后插 "# "）
        let (out, bs, be) = toggle_comment_lines(text, 0, 1);
        assert_eq!(out, "# a = 1\n\t# b = 2\nc");
        // 块偏移 = 第一行起点 0 → 第二行行尾（"# a = 1"=7 + '\n'=1 + "\t# b = 2"=8）
        assert_eq!((bs, be), (0, 7 + 1 + 8));
    }

    #[test]
    fn 注释切换_已注释解除() {
        let text = "# a = 1\n#b = 2\nx = 3";
        // 全部已注释 → 解除；'#' 后至多一个空格一并去掉
        let (out, _, _) = toggle_comment_lines(text, 0, 1);
        assert_eq!(out, "a = 1\nb = 2\nx = 3");
    }

    #[test]
    fn 注释切换_混合时统一加注() {
        // 一行已注释一行未注释 → 整块加注（已注释行保持原样不再叠加）
        let text = "# a\nb";
        let (out, _, _) = toggle_comment_lines(text, 0, 1);
        assert_eq!(out, "# # a\n# b");
    }

    #[test]
    fn 注释切换_空行与单行() {
        // 全空行选择 → 加注（VSCode 行为：给空行插注释符）
        let (out, _, _) = toggle_comment_lines("", 0, 0);
        assert_eq!(out, "# ");
        // 含空行的块加注：空行同样加
        let (out, _, _) = toggle_comment_lines("a\n\nc", 0, 2);
        assert_eq!(out, "# a\n# \n# c");
        // 越界行号收敛
        let (out, _, _) = toggle_comment_lines("a", 5, 9);
        assert_eq!(out, "# a");
    }

    #[test]
    fn 注释切换_块偏移覆盖选中行() {
        // 切换后偏移必须覆盖第一行起点到最后行终点（供光标选区恢复）
        let text = "x\na = 1\nb = 2\ny";
        let (out, bs, be) = toggle_comment_lines(text, 1, 2);
        assert_eq!(out, "x\n# a = 1\n# b = 2\ny");
        let out_chars: Vec<char> = out.chars().collect();
        assert_eq!(out_chars[bs], '#'); // bs 指向第二行行首
        assert_eq!(&out_chars[bs..be].iter().collect::<String>(), "# a = 1\n# b = 2");
    }
}
