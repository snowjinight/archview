//! encode CLI 壳（doc/架构设计/15_encode引擎.md §15.5）：`archview encode <输入...> [-o out] [-p pw] [-q]`
//!
//! 输入语义总纲「显式选择即精确」：
//! - ≥1 个帧文件 = 精确模式：多文件参数是**一个序列的成员**（区别于 tidy 的
//!   多任务语义），须同目录且命中同一 [[raw.demux]] 预设；自然序排序后编码
//!   （SendTo 多选传参顺序不可靠，壳内必须重排）
//! - 目录 = 唯一族校验：顶层恰一族 → 全族；多族 → 报错列各族清单
//! - 压缩包（zip/7z/rar）= VFS 打开 + 条目名匹配预设，免解压直编
//!
//! 编码参数不进命令行（config.toml [encode] 节）；输出缺省 = 序列所在目录 +
//! 首帧基名（dump_0000.nv12 → dump_0000.mp4），已存在一律拒绝覆盖。
//! SendTo 32767 字符上限防呆：自然序后做编号连续性校验，缺号**警告不中止**
//! （稀疏 dump 合法；超限静默截断才是要防的——丢整倍数尾帧引擎拦不住）。
//!
//! Out / msgbox 为 vartidy 同款自实现副本（vartidy 不为 encode 改一行）。

use crate::config::{Config, EncodeConfig, RawDemux};
use crate::encode::{encode_raw_sequence, EncodeFormat, EncodeOptions};
use crate::vfs::dir_vfs::DirVFS;
use crate::vfs::{open_with_password_probe, ArchiveVFS, VirtualEntry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

const USAGE: &str = r#"archview encode — 帧序列 dump → 视频（libmpv 编码）

用法: archview encode <输入...> [-o <输出文件>] [-p <密码>] [-q]

输入（三种，不可混用）:
  帧文件   一个或多个 dump 文件（同一目录）= 精确模式：所见即所选，
           多文件视为一个序列的成员，按自然序（1/2/10）排序后编码
  目录     唯一族校验：目录下恰好一族 [[raw.demux]] 命中的文件 → 全族编码；
           多族时报错并列各族清单（选中其中一族任一帧文件重发即可）
  压缩包   zip / 7z / rar：包内条目名匹配预设，免解压直编

选项:
  -o, --out <文件>     输出路径（容器由扩展名推断，如 .mp4 / .mkv；无扩展名
                       自动补 .mp4）。缺省 = 序列所在目录 + 首帧基名
                       （dump_0000.nv12 → dump_0000.mp4）。已存在拒绝覆盖
  -p, --password <密码>  压缩包密码（config.toml passwords 兜底，逐候选尝试）
  -q, --quiet          静默：不弹结果窗（有控制台时仍打印）
  -h, --help           本帮助

编码参数在 config.toml [encode] 节（vcodec / crf / preset / ovcopts）。
「发送到」快捷方式: 运行 sendto_setup.bat 一键创建（Archview encode 项）。
提示: 大序列请发目录——「发送到」多文件受命令行 32767 字符上限约束，
      超限时 explorer 可能静默截断（丢尾帧且无提示）"#;

// ============================================================
// 控制台输出 / 弹窗（vartidy 同款自实现副本）
// ============================================================

/// CLI 输出：GUI 子系统 exe 从终端启动无控制台——附加父进程控制台经 CONOUT$ 直写
struct Out {
    con: Option<std::fs::File>,
    had_console: bool,
    /// 是否写过 `\r` 进度行（结束时补换行收尾）
    wrote_progress: AtomicBool,
}

impl Out {
    fn attach() -> Self {
        unsafe {
            let has = !windows_sys::Win32::System::Console::GetConsoleWindow().is_null();
            if has {
                return Self {
                    con: None,
                    had_console: true,
                    wrote_progress: AtomicBool::new(false),
                };
            }
            let attached = windows_sys::Win32::System::Console::AttachConsole(
                windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
            ) != 0;
            let con = if attached {
                std::fs::File::create("CONOUT$").ok()
            } else {
                None
            };
            Self {
                con,
                had_console: false,
                wrote_progress: AtomicBool::new(false),
            }
        }
    }

    fn line(&self, s: &str) {
        self.end_progress();
        use std::io::Write;
        match &self.con {
            // &File 本身实现 Write（内部带锁），重新绑定后才可作为可变接收者
            Some(f) => {
                let mut f = f;
                let _ = writeln!(f, "{s}");
            }
            None => println!("{s}"),
        }
    }

    /// 进度行：`\r` 原地刷新（仅在确有可见控制台时输出——SendTo/双击
    /// 附加失败时静默，结果靠 msgbox）
    fn progress(&self, pct: f64) {
        use std::io::Write;
        match &self.con {
            Some(f) => {
                let mut f = f;
                let _ = write!(f, "\r编码中 {pct:5.1}%");
                let _ = f.flush();
                self.wrote_progress.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            None if self.had_console => {
                print!("\r编码中 {pct:5.1}%");
                let _ = std::io::stdout().flush();
                self.wrote_progress.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            None => {}
        }
    }

    /// 结束 `\r` 进度行（打过才补换行；后续 line 输出不被进度行吞掉）
    fn end_progress(&self) {
        if self.wrote_progress.swap(false, std::sync::atomic::Ordering::Relaxed) {
            match &self.con {
                Some(f) => {
                    use std::io::Write;
                    let mut f = f;
                    let _ = writeln!(f);
                }
                None => println!(),
            }
        }
    }
}

fn msgbox(text: &str, error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let t = wide(text);
    let c = wide("Archview Encode");
    let icon = if error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | icon);
    }
}

// ============================================================
// 参数解析
// ============================================================

#[derive(Debug)]
struct ParsedArgs {
    inputs: Vec<String>,
    out: Option<String>,
    passwords: Vec<String>,
    quiet: bool,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut p = ParsedArgs {
        inputs: Vec::new(),
        out: None,
        passwords: Vec::new(),
        quiet: false,
        help: false,
    };
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => p.help = true,
            "-q" | "--quiet" => p.quiet = true,
            "-o" | "--out" => {
                i += 1;
                p.out = Some(args.get(i).ok_or("--out 需要一个文件参数")?.clone());
            }
            "-p" | "--password" => {
                i += 1;
                p.passwords
                    .push(args.get(i).ok_or("--password 需要一个密码参数")?.clone());
            }
            s if s.starts_with('-') => return Err(format!("未知选项: {s}")),
            s => p.inputs.push(s.to_string()),
        }
        i += 1;
    }
    Ok(p)
}

/// 密码列表合并：CLI -p 在前（显式指定优先尝试），config passwords 在后；
/// 去重保序、剔除空串（tidy 同惯例）
fn merge_passwords(cli: &[String], cfg: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in cli.iter().chain(cfg.iter()) {
        if !p.is_empty() && !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

// ============================================================
// 输入分类（三通道互斥，混合报错）
// ============================================================

enum Input {
    /// ≥1 个普通帧文件（同目录）
    Files(Vec<PathBuf>),
    /// 恰 1 个目录
    Dir(PathBuf),
    /// 恰 1 个压缩包
    Archive(PathBuf),
}

fn is_archive_ext(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("zip") | Some("7z") | Some("rar")
    )
}

fn classify(inputs: &[PathBuf]) -> Result<Input, String> {
    for p in inputs {
        if !p.exists() {
            return Err(format!("输入不存在: {}", p.display()));
        }
    }
    let (dirs, files): (Vec<_>, Vec<_>) = inputs.iter().cloned().partition(|p| p.is_dir());
    if dirs.len() > 1 {
        return Err("一次只能编码一个目录（一个目录 = 一个序列）".into());
    }
    if !dirs.is_empty() && !files.is_empty() {
        return Err(
            "文件与目录/压缩包混合输入不支持：请单独发送帧文件、目录或压缩包".into(),
        );
    }
    if let Some(d) = dirs.into_iter().next() {
        return Ok(Input::Dir(d));
    }
    let (archives, plain): (Vec<_>, Vec<_>) = files.into_iter().partition(|p| is_archive_ext(p));
    if archives.len() > 1 {
        return Err("一次只能编码一个压缩包（压缩包内所有命中预设的条目即一个序列）".into());
    }
    if archives.len() == 1 && !plain.is_empty() {
        return Err("压缩包与帧文件混合输入不支持：请单独发送".into());
    }
    if let Some(a) = archives.into_iter().next() {
        return Ok(Input::Archive(a));
    }
    Ok(Input::Files(plain))
}

// ============================================================
// 序列收集（三通道 → 统一 vfs + 自然序 entries + 格式 + 默认输出）
// ============================================================

/// 命中的 [[raw.demux]] 预设声明索引（族分组键）。raw_demux 已做路径剥离
/// + 声明序取首个命中；分组需要索引，按指针等价回查（返回引用唯一）
pub(crate) fn preset_index(cfg: &Config, name: &str) -> Option<usize> {
    let hit = cfg.raw_demux(name)?;
    cfg.raw.demux.iter().position(|d| std::ptr::eq(d, hit))
}

/// 收敛后的待编码序列
struct Sequence {
    vfs: Arc<dyn ArchiveVFS>,
    /// 已按自然序排列（引擎契约：壳负责排序）
    entries: Vec<VirtualEntry>,
    fmt: EncodeFormat,
    /// 默认输出：序列所在目录 + 首帧基名 + .mp4
    out_default: PathBuf,
    /// 编号断点等预警（警告不中止）
    warnings: Vec<String>,
}

/// vfs 是 dyn 字段不能 derive——测试 unwrap_err 需要，手写精简实现
impl std::fmt::Debug for Sequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sequence")
            .field("entries", &self.entries)
            .field("fmt", &self.fmt)
            .field("out_default", &self.out_default)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

/// 文件按命中预设分族（滤目录条目，组内自然序）。
/// 返回 (预设索引 → 条目列表)，键序 = 声明序（报错清单稳定）
pub(crate) fn families(cfg: &Config, files: Vec<VirtualEntry>) -> BTreeMap<usize, Vec<VirtualEntry>> {
    let mut groups: BTreeMap<usize, Vec<VirtualEntry>> = BTreeMap::new();
    for e in files {
        if e.is_dir {
            continue;
        }
        if let Some(i) = preset_index(cfg, &e.name) {
            groups.entry(i).or_default().push(e);
        }
    }
    for g in groups.values_mut() {
        g.sort_by(|a, b| alphanumeric_sort::compare_str(&a.name, &b.name));
    }
    groups
}

/// 单族提取 + 报错文案（目录/压缩包通道共用；精确模式已在外层逐文件校验，
/// 理论上不会再触发多族，兜底同文案）
fn single_family(
    cfg: &Config,
    groups: BTreeMap<usize, Vec<VirtualEntry>>,
    where_: &str,
) -> Result<(RawDemux, Vec<VirtualEntry>), String> {
    match groups.len() {
        0 => Err(format!(
            "{where_}没有命中 [[raw.demux]] 预设的文件（预设声明见 config.toml）"
        )),
        1 => {
            let (i, g) = groups.into_iter().next().unwrap();
            Ok((cfg.raw.demux[i].clone(), g))
        }
        _ => {
            let mut lines = vec![format!("{where_}有多族帧序列，无法确定编码目标：")];
            for (i, g) in &groups {
                let p = &cfg.raw.demux[*i];
                lines.push(format!(
                    "  - match={}：{} 个文件（{}x{} {} @ {}fps）",
                    p.r#match,
                    g.len(),
                    p.width,
                    p.height,
                    p.pixfmt,
                    p.fps
                ));
            }
            lines.push("选中其中一族任一帧文件重发即可精确指定".into());
            Err(lines.join("\n"))
        }
    }
}

pub(crate) fn encode_format(p: &RawDemux) -> EncodeFormat {
    EncodeFormat {
        pixfmt: p.pixfmt.clone(),
        width: p.width,
        height: p.height,
        fps: p.fps,
        bit_shift: p.bit_shift,
    }
}

/// 默认输出名：基目录 + 首帧条目基名（去扩展名）+ .mp4。
/// 首帧 name 是 VFS 相对路径（可能带子目录前缀），输出跟随其所在子目录
pub(crate) fn default_output(base: &Path, first_entry_name: &str) -> PathBuf {
    let p = Path::new(first_entry_name);
    let dir = base.join(p.parent().unwrap_or_else(|| Path::new("")));
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned());
    let stem = stem.as_deref().unwrap_or("output");
    dir.join(format!("{stem}.mp4"))
}

/// 精确模式：多文件 = 一个序列的成员。校验同目录 + 同族，DirVFS 覆盖收集
fn collect_files(cfg: &Config, paths: &[PathBuf]) -> Result<Sequence, String> {
    if paths.is_empty() {
        return Err("输入为空".into());
    }
    // 同目录校验：canonicalize 消解 ..\、正反斜杠与相对/绝对写法差异
    let mut canon = Vec::with_capacity(paths.len());
    for p in paths {
        canon.push(
            p.canonicalize()
                .map_err(|e| format!("无法访问 {}: {e}", p.display()))?,
        );
    }
    let parent = canon[0].parent().unwrap_or_else(|| Path::new(""));
    if canon.iter().skip(1).any(|p| p.parent() != Some(parent)) {
        return Err(
            "精确模式的多个文件必须在同一目录（多文件参数 = 一个序列的成员）；\
             跨目录内容请改用目录/压缩包输入，或分次编码"
                .into(),
        );
    }
    // 预设族一致性：逐文件给出精确文案（未命中 / 混族）
    let mut fam: Option<usize> = None;
    for p in paths {
        let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        match preset_index(cfg, &name) {
            None => {
                return Err(format!(
                    "文件 [{name}] 未命中任何 [[raw.demux]] 预设（config.toml）"
                ));
            }
            Some(i) => match fam {
                None => fam = Some(i),
                Some(prev) if prev == i => {}
                Some(prev) => {
                    let a = &cfg.raw.demux[prev].r#match;
                    let b = &cfg.raw.demux[i].r#match;
                    return Err(format!(
                        "选中的文件命中多个不同预设（{a} 与 {b}），不是同一序列；\
                         请只选同一族的文件"
                    ));
                }
            },
        }
    }
    // DirVFS 收集：谓词 = 小写文件名集合（Windows 目录项大小写与传参可能不同形态）
    let names_lc: Vec<String> = paths
        .iter()
        .map(|p| {
            p.file_name()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        })
        .collect();
    let root = paths[0]
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let dv = DirVFS::open(&root, |n| names_lc.iter().any(|x| x == &n.to_ascii_lowercase()), 1)
        .map_err(|e| e.to_string())?;
    let entries = dv.list();
    if entries.len() != paths.len() {
        return Err(format!(
            "目录收集到 {} 个文件，与输入 {} 个不符（文件读取异常）",
            entries.len(),
            paths.len()
        ));
    }
    let groups = families(cfg, entries);
    let (preset, entries) = single_family(cfg, groups, "所选文件")?;
    Ok(Sequence {
        out_default: default_output(&root, &entries[0].name),
        warnings: numbering_gaps(&entries),
        fmt: encode_format(&preset),
        vfs: Arc::new(dv),
        entries,
    })
}

/// 目录通道：唯一族校验（顶层直接子文件，防误发盘根全盘扫描）
fn collect_dir(cfg: &Config, dir: &Path) -> Result<Sequence, String> {
    let dv = DirVFS::open(dir, |n| cfg.raw_demux(n).is_some(), 1).map_err(|e| e.to_string())?;
    let entries = dv.list();
    let groups = families(cfg, entries);
    let (preset, entries) = single_family(cfg, groups, "目录内")?;
    Ok(Sequence {
        out_default: default_output(dir, &entries[0].name),
        warnings: numbering_gaps(&entries),
        fmt: encode_format(&preset),
        vfs: Arc::new(dv),
        entries,
    })
}

/// 压缩包通道：免解压直编（预设探测打开，加密条目提前拦截）
fn collect_archive(
    cfg: &Config,
    path: &Path,
    candidates: &[String],
) -> Result<Sequence, String> {
    let (v, _pw) =
        open_with_password_probe(path, candidates).map_err(|e| {
            format!("打开压缩包失败 {}: {e}", path.display())
        })?;
    let vfs: Arc<dyn ArchiveVFS> = v.into();
    let entries = vfs.list();
    if let Some(e) = entries.iter().find(|e| e.encrypted) {
        return Err(format!(
            "包内条目加密（暂不支持，请先解压）：[{}]",
            e.name
        ));
    }
    let groups = families(cfg, entries);
    let (preset, entries) = single_family(cfg, groups, "压缩包内")?;
    let base = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    Ok(Sequence {
        out_default: default_output(&base, &entries[0].name),
        warnings: numbering_gaps(&entries),
        fmt: encode_format(&preset),
        vfs,
        entries,
    })
}

// ============================================================
// 编号连续性校验（SendTo 32767 截断防呆：警告不中止）
// ============================================================

/// 文件名尾部数字段（dump_0000.nv12 → Some(0)）；无尾部数字 → None。
/// stem 按最后一个 '.' 剥离（dump.0000.nv12 → dump.0000 → 0）
fn trailing_number(name: &str) -> Option<u64> {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    // 尾部连续数字段：定位最后一个非数字字符之后的部分
    let pos = stem.rfind(|c: char| !c.is_ascii_digit()).map(|i| i + 1).unwrap_or(0);
    let digits = &stem[pos..];
    if digits.is_empty() { None } else { digits.parse().ok() }
}

/// 自然序序列的编号断点：全体成员可解析出尾部编号才校验（无编号轴 = 稀疏
/// 命名之外的合法形态，跳过）。缺号区间可能是 SendTo 命令行被静默截断丢的
/// 尾帧，也可能是合法稀疏 dump——报告断点由用户判断
pub(crate) fn numbering_gaps(entries: &[VirtualEntry]) -> Vec<String> {
    let nums: Option<Vec<u64>> = entries.iter().map(|e| trailing_number(&e.name)).collect();
    let Some(nums) = nums else {
        return Vec::new();
    };
    let mut gaps = Vec::new();
    for i in 1..nums.len() {
        if nums[i] > nums[i - 1] + 1 {
            gaps.push(format!(
                "[{}] 与 [{}] 之间缺编号 {}~{}",
                entries[i - 1].name,
                entries[i].name,
                nums[i - 1] + 1,
                nums[i] - 1
            ));
        }
    }
    gaps
}

// ============================================================
// 编码选项组装 + 主流程
// ============================================================

/// [encode] 节 → 引擎选项。crf/preset 组装为 ovcopts 默认项，用户 ovcopts
/// 追加在后（mpv ovcopts 同键后值覆盖前值 = 用户显式项以用户为准；
/// 引擎再统一追加 bf=0 兜底，用户显式 bf/bframes 除外——doc/架构设计/15_encode引擎.md §15.2）
pub(crate) fn encode_options(ec: &EncodeConfig) -> EncodeOptions {
    let mut parts = vec![format!("crf={}", ec.crf), format!("preset={}", ec.preset)];
    let user = ec.ovcopts.trim();
    if !user.is_empty() {
        parts.push(user.to_string());
    }
    EncodeOptions {
        vcodec: ec.vcodec.clone(),
        ovcopts: Some(parts.join(",")),
    }
}

/// 人类可读大小（B/KB/MB/GB，一位小数；tidy 同款格式）
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    match bytes {
        b if b < KB => format!("{b} B"),
        b if b < MB => format!("{:.1} KB", b as f64 / KB as f64),
        b if b < GB => format!("{:.1} MB", b as f64 / MB as f64),
        b => format!("{:.1} GB", b as f64 / GB as f64),
    }
}

/// CLI 主入口（main.rs 按 `encode` 子命令分发）。
/// 返回进程退出码：0 = 完成；1 = 编码失败；2 = 用法/输入错误
pub fn cli_main(args: &[String]) -> i32 {
    let out = Out::attach();

    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            out.line(&format!("参数错误: {e}\n\n{USAGE}"));
            // 解析失败时 quiet 状态未知，从原始参数探测
            if !args.iter().any(|a| a == "-q" || a == "--quiet") {
                msgbox(&format!("参数错误: {e}"), true);
            }
            return 2;
        }
    };
    let quiet = parsed.quiet;
    if parsed.help {
        out.line(USAGE);
        return 0;
    }
    if parsed.inputs.is_empty() {
        out.line(&format!("缺少输入（帧文件 / 目录 / 压缩包）\n\n{USAGE}"));
        if !quiet {
            msgbox("缺少输入：请在命令后指定帧文件、目录或压缩包，或用「发送到」发送。", true);
        }
        return 2;
    }

    let cfg = match Config::load_or_create() {
        Ok(c) => c,
        Err(e) => {
            out.line(&format!("config.toml 加载失败: {e}"));
            if !quiet {
                msgbox(&format!("config.toml 加载失败: {e}"), true);
            }
            return 2;
        }
    };
    crate::vfs::set_codepage(&cfg.codepage);

    let inputs: Vec<PathBuf> = parsed.inputs.iter().map(PathBuf::from).collect();
    let seq = match classify(&inputs) {
        Err(e) => Err(e),
        Ok(Input::Files(ps)) => collect_files(&cfg, &ps),
        Ok(Input::Dir(d)) => collect_dir(&cfg, &d),
        Ok(Input::Archive(a)) => {
            let pws = merge_passwords(&parsed.passwords, &cfg.passwords);
            collect_archive(&cfg, &a, &pws)
        }
    };
    let seq = match seq {
        Ok(s) => s,
        Err(e) => {
            out.line(&format!("输入错误: {e}"));
            if !quiet {
                msgbox(&format!("输入错误: {e}"), true);
            }
            return 2;
        }
    };

    // 输出路径：-o 优先；无扩展名补 .mp4（mpv o= 按扩展名推断容器）
    let mut out_path = match parsed.out {
        Some(o) => PathBuf::from(o),
        None => seq.out_default.clone(),
    };
    if out_path.extension().is_none() {
        out_path.set_extension("mp4");
    }
    if out_path.exists() {
        let msg = format!(
            "输出文件已存在（拒绝覆盖）：{}\n请用 -o 指定其他路径，或移动/删除原文件",
            out_path.display()
        );
        out.line(&msg);
        if !quiet {
            msgbox(&msg, true);
        }
        return 2;
    }

    // 编号断点预警（SendTo 32767 截断防呆）：警告不中止
    for w in &seq.warnings {
        out.line(&format!("警告: 编号不连续——{w}"));
    }

    out.line(&format!(
        "编码 {} 个文件（{}x{} {} @ {}fps，vcodec={}）\n→ {}",
        seq.entries.len(),
        seq.fmt.width,
        seq.fmt.height,
        seq.fmt.pixfmt,
        seq.fmt.fps,
        cfg.encode.vcodec,
        out_path.display()
    ));

    // CLI 单线程直跑（阻塞直至完成）；无交互取消（Ctrl+C 直接终止进程）
    let cancel = AtomicBool::new(false);
    let opts = encode_options(&cfg.encode);
    let t0 = Instant::now();
    let result = encode_raw_sequence(
        &cfg.dll_path(),
        &seq.vfs,
        &seq.entries,
        &seq.fmt,
        &out_path,
        &opts,
        &cancel,
        &|pct| out.progress(pct),
    );
    out.end_progress();

    match result {
        Ok(st) => {
            let text = format!(
                "编码完成：{} 帧（{} 个文件，{}），耗时 {:.1}s\n输出: {}",
                st.frames,
                st.files,
                human_size(st.total_bytes),
                t0.elapsed().as_secs_f64(),
                out_path.display()
            );
            out.line(&text);
            if !out.had_console && !quiet {
                msgbox(&text, false);
            }
            0
        }
        Err(e) => {
            let text = format!("编码失败: {e}");
            out.line(&text);
            if !out.had_console && !quiet {
                msgbox(&text, true);
            }
            1
        }
    }
}

// ============================================================
// 测试（纯逻辑：收集/分组/编号/组装，不启动 mpv）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12_cfg() -> Config {
        let mut cfg = Config::default();
        // 清掉内置三条默认预设（nv12/yuv400/raw）：serde 语义下用户 TOML
        // 写了 [[raw.demux]] 即整体替换默认数组；程序内 push 才会叠加，
        // 同名 *.nv12 会被默认宽高 0 的条目按声明序抢先命中
        cfg.raw.demux.clear();
        cfg.raw.demux.push(RawDemux {
            r#match: "*.nv12".into(),
            pixfmt: "nv12".into(),
            width: 64,
            height: 48,
            fps: 25.0,
            bit_shift: 0,
        });
        cfg
    }

    fn entry(name: &str, size: u64) -> VirtualEntry {
        VirtualEntry {
            name: name.into(),
            size,
            is_dir: false,
            encrypted: false,
            crc: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("enccli_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parse_args_basic_and_errors() {
        let p = parse_args(&[
            "a.nv12".into(),
            "b.nv12".into(),
            "-o".into(),
            "out.mp4".into(),
            "-p".into(),
            "pw".into(),
            "-q".into(),
        ])
        .unwrap();
        assert_eq!(p.inputs, ["a.nv12", "b.nv12"]);
        assert_eq!(p.out.as_deref(), Some("out.mp4"));
        assert_eq!(p.passwords, ["pw"]);
        assert!(p.quiet && !p.help);

        assert!(parse_args(&["-o".into()]).is_err());
        assert!(parse_args(&["-p".into()]).is_err());
        assert_eq!(
            parse_args(&["--bogus".into()]).unwrap_err(),
            "未知选项: --bogus"
        );
    }

    #[test]
    fn merge_passwords_dedup_keeps_order() {
        let m = merge_passwords(&["a".into(), "".into(), "b".into()], &["b".into(), "c".into()]);
        assert_eq!(m, ["a", "b", "c"]);
    }

    #[test]
    fn trailing_number_forms() {
        assert_eq!(trailing_number("dump_0000.nv12"), Some(0));
        assert_eq!(trailing_number("seq_00129.yuv"), Some(129));
        assert_eq!(trailing_number("a12b34.bin"), Some(34));
        assert_eq!(trailing_number("dump.0007.raw"), Some(7));
        assert_eq!(trailing_number("no_digits.nv12"), None);
        assert_eq!(trailing_number("trailing_not_digit_12a.nv12"), None);
    }

    #[test]
    fn numbering_gaps_reports_and_skips() {
        // 缺号：10 与 20 之间缺 11~19
        let es = vec![entry("f_10.nv12", 1), entry("f_20.nv12", 1)];
        let g = numbering_gaps(&es);
        assert_eq!(g.len(), 1);
        assert!(g[0].contains("11~19"), "{}", g[0]);

        // 连续：无警告
        let es = vec![entry("f_1.nv12", 1), entry("f_2.nv12", 1), entry("f_3.nv12", 1)];
        assert!(numbering_gaps(&es).is_empty());

        // 无编号轴（一个成员无尾部数字即跳过整组）
        let es = vec![entry("f_1.nv12", 1), entry("x.nv12", 1), entry("f_5.nv12", 1)];
        assert!(numbering_gaps(&es).is_empty());

        // 倒序编号不算断点（合法倒序 dump）
        let es = vec![entry("f_5.nv12", 1), entry("f_3.nv12", 1)];
        assert!(numbering_gaps(&es).is_empty());
    }

    #[test]
    fn encode_options_assembly_user_last() {
        let mut ec = EncodeConfig::default();
        let o = encode_options(&ec);
        assert_eq!(o.vcodec, "libx264");
        assert_eq!(o.ovcopts.as_deref(), Some("crf=23,preset=medium"));

        ec.crf = 18;
        ec.preset = "slow".into();
        ec.ovcopts = "bf=3,crf=16".into();
        let o = encode_options(&ec);
        // 用户项追加在后（mpv 同键后值覆盖 → crf 生效 16，bf=3 抑制引擎 bf=0）
        assert_eq!(o.ovcopts.as_deref(), Some("crf=18,preset=slow,bf=3,crf=16"));
    }

    #[test]
    fn classify_mix_and_types() {
        let d = temp_dir("cls");
        let f1 = d.join("a.nv12");
        let f2 = d.join("b.nv12");
        std::fs::write(&f1, b"x").unwrap();
        std::fs::write(&f2, b"x").unwrap();
        let sub = d.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let zip = d.join("pack.zip");
        std::fs::write(&zip, b"x").unwrap();

        // 文件组
        match classify(&[f1.clone(), f2.clone()]).unwrap() {
            Input::Files(v) => assert_eq!(v.len(), 2),
            _ => panic!("应分类为 Files"),
        }
        // 目录 / 压缩包
        assert!(matches!(classify(&[sub.clone()]), Ok(Input::Dir(_))));
        assert!(matches!(classify(&[zip.clone()]), Ok(Input::Archive(_))));
        // 不存在
        assert!(classify(&[d.join("nope.nv12")]).is_err());
        // 混合
        assert!(classify(&[f1.clone(), sub.clone()]).is_err());
        assert!(classify(&[f1.clone(), zip.clone()]).is_err());
        let sub2 = d.join("sub2");
        std::fs::create_dir(&sub2).unwrap();
        assert!(classify(&[sub.clone(), sub2]).is_err());
        let zip2 = d.join("p2.zip");
        std::fs::write(&zip2, b"x").unwrap();
        assert!(classify(&[zip.clone(), zip2]).is_err());
    }

    #[test]
    fn collect_files_precise_and_default_output() {
        let cfg = nv12_cfg();
        let d = temp_dir("cf");
        // 自然序打乱写入（f_2 在 f_10 前、f_1 最后）——收集后必须重排
        for n in ["f_2.nv12", "f_10.nv12", "f_1.nv12"] {
            std::fs::write(d.join(n), vec![0u8; 64 * 48 * 3 / 2]).unwrap();
        }
        std::fs::write(d.join("note.txt"), b"ignore").unwrap();
        let paths = vec![d.join("f_10.nv12"), d.join("f_1.nv12"), d.join("f_2.nv12")];
        let s = collect_files(&cfg, &paths).unwrap();
        assert_eq!(
            s.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["f_1.nv12", "f_2.nv12", "f_10.nv12"]
        );
        // 默认输出 = 目录 + 首帧基名 + .mp4
        assert_eq!(s.out_default, d.join("f_1.mp4"));
        assert_eq!(s.fmt.width, 64);
        // 编号断点（1,2,10 → 缺 3~9）：恰一条警告；自然序重排语义见上
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("3~9"), "{:?}", s.warnings);

        // 未命中预设
        let e = collect_files(&cfg, &[d.join("note.txt")]).unwrap_err();
        assert!(e.contains("未命中"), "{e}");
        // 跨目录
        let d2 = temp_dir("cf2");
        std::fs::write(d2.join("x.nv12"), vec![0u8; 64 * 48 * 3 / 2]).unwrap();
        let e = collect_files(&cfg, &[d.join("f_1.nv12"), d2.join("x.nv12")]).unwrap_err();
        assert!(e.contains("同一目录"), "{e}");
    }

    #[test]
    fn collect_dir_single_and_multi_family() {
        let cfg = nv12_cfg();
        let d = temp_dir("cd");
        for n in ["g_1.nv12", "g_2.nv12"] {
            std::fs::write(d.join(n), vec![0u8; 8]).unwrap();
        }
        let s = collect_dir(&cfg, &d).unwrap();
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.out_default, d.join("g_1.mp4"));

        // 多族：第二条预设命中的 gray 文件混入
        let mut cfg2 = cfg.clone();
        cfg2.raw.demux.push(RawDemux {
            r#match: "cam_*.gray".into(),
            pixfmt: "gray".into(),
            width: 32,
            height: 32,
            fps: 30.0,
            bit_shift: 0,
        });
        std::fs::write(d.join("cam_0.gray"), vec![0u8; 8]).unwrap();
        let e = collect_dir(&cfg2, &d).unwrap_err();
        assert!(e.contains("多族"), "{e}");
        assert!(e.contains("*.nv12") && e.contains("cam_*.gray"), "{e}");

        // 空目录（无命中文件）
        let d3 = temp_dir("cd3");
        assert!(collect_dir(&cfg, &d3).unwrap_err().contains("没有命中"));
    }

    #[test]
    fn collect_archive_zip_family_and_encrypted() {
        use std::io::Write as _;
        let cfg = nv12_cfg();
        let d = temp_dir("ca");
        let zp = d.join("pack.zip");
        let file = std::fs::File::create(&zp).unwrap();
        let mut w = zip::write::ZipWriter::new(file);
        let zo = zip::write::SimpleFileOptions::default();
        for n in ["z_2.nv12", "z_1.nv12"] {
            w.start_file(n, zo).unwrap();
            w.write_all(&vec![0u8; 8]).unwrap();
        }
        w.start_file("readme.txt", zo).unwrap();
        w.write_all(b"hi").unwrap();
        w.finish().unwrap();

        let s = collect_archive(&cfg, &zp, &[]).unwrap();
        assert_eq!(
            s.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["z_1.nv12", "z_2.nv12"]
        );
        // 默认输出在压缩包所在目录
        assert_eq!(s.out_default, d.join("z_1.mp4"));

        // 包内无命中族
        let zp2 = d.join("empty.zip");
        let f2 = std::fs::File::create(&zp2).unwrap();
        let mut w2 = zip::write::ZipWriter::new(f2);
        w2.start_file("x.txt", zo).unwrap();
        w2.write_all(b"hi").unwrap();
        w2.finish().unwrap();
        assert!(collect_archive(&cfg, &zp2, &[]).unwrap_err().contains("没有命中"));
    }

    #[test]
    fn default_output_keeps_subdir() {
        // 首帧条目带子目录前缀时输出跟随其所在子目录
        let p = default_output(Path::new("E:/seq"), "day1/h_0000.nv12");
        assert_eq!(p, PathBuf::from("E:/seq/day1/h_0000.mp4"));
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
        assert!(human_size(5 * 1024_u64.pow(3)).starts_with("5.0 GB"));
    }
}
