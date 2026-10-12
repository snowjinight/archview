//! VarTidy：var 资源包增量整理流水线（CLI 子命令 `archview tidy`，ARCHITECTURE.md 第 12 章）。
//!
//! 核心原则（不可违反）：
//! 1. 仓库只读 + 增量：仓库已有文件/文件夹一律不动，仅创建新作者目录、写入仓库没有的新 var
//! 2. 同名冲突仓库版胜出：代码中不存在覆盖/替换路径（rename_no_replace 是最后一道闸）
//! 3. 无效文件不落盘：只进报告
//! 4. 零临时全量落盘：按 entry 直写仓库目标路径（staging 同目录 + 原子改名）

pub mod classify;

use crate::vfs::{open_archive, ArchiveVFS, VirtualEntry, VfsError};
use classify::{classify_file_name, ClassifyResult};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 仓库写入前的磁盘余量缓冲（与播放器解压预检同款策略）
const DISK_HEADROOM_MB: u64 = 64;
/// 进度输出节流间隔
const PROGRESS_THROTTLE: Duration = Duration::from_millis(400);

// ============================================================
// 选项 / 结果
// ============================================================

pub struct TidyOptions {
    /// var 仓库根目录（必须已存在；整理器不创建仓库、不改其中已有内容）
    pub repo_dir: PathBuf,
    /// 预演模式：只做分类 + 两级去重检查并出报告，不写任何文件
    pub dry_run: bool,
    /// 报告输出目录；None = 自动（首个输入所在目录，保持仓库纯净）
    pub report_dir: Option<PathBuf>,
    /// 加密包密码候选（按序，已去重）：CLI -p 在前，config vartidy.passwords 在后。
    /// 实际尝试序列 = [无密码] + 此列表——绝大多数包不加密，无密码优先零成本直通
    pub passwords: Vec<String>,
}

#[derive(Debug, Default)]
pub struct TidySummary {
    pub archives: usize,
    pub added: usize,
    pub added_bytes: u64,
    pub repo_exists: usize,
    pub in_archive_dup: usize,
    pub crc_conflict: usize,
    pub invalid: usize,
    pub failed_files: usize,
    pub failed_archives: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Added,
    RepoExists,
    InArchiveDup,
    CrcConflict,
    Invalid,
    Failed,
}

impl LineKind {
    fn label(&self) -> &'static str {
        match self {
            LineKind::Added => "新增",
            LineKind::RepoExists => "仓库已有",
            LineKind::InArchiveDup => "包内重复",
            LineKind::CrcConflict => "同名不同内容",
            LineKind::Invalid => "无效",
            LineKind::Failed => "失败",
        }
    }
}

struct ReportLine {
    kind: LineKind,
    archive: String,
    entry: String,
    note: String,
}

impl ReportLine {
    fn new(kind: LineKind, archive: &str, entry: &str, note: impl Into<String>) -> Self {
        Self {
            kind,
            archive: archive.to_string(),
            entry: entry.to_string(),
            note: note.into(),
        }
    }
}

/// 一次整理运行的结果
pub struct TidyRun {
    pub summary: TidySummary,
    pub report_path: Option<PathBuf>,
}

// ============================================================
// 输入展开
// ============================================================

fn is_archive(p: &Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "zip" | "7z" | "rar")
}

/// 输入展开：文件 → 单个包；目录 → 递归收集压缩包（分卷 RAR 同族只保留首卷，
/// 与 GUI「连续看包」的 archives_in_dir 同策略）
fn expand_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in inputs {
        if p.is_dir() {
            collect_archives(p, &mut out);
        } else if is_archive(p) {
            out.push(p.clone());
        }
    }
    out
}

fn collect_archives(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            subdirs.push(p);
        } else if is_archive(&p) {
            files.push(p);
        }
    }
    // 本层按文件名排序后入列（分卷族去重需要 part1 排最前）
    files.sort_by_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    let mut family_seen: HashSet<String> = HashSet::new();
    for p in files {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        match crate::vfs::split_rar_family(&name) {
            Some(f) => {
                if family_seen.insert(f) {
                    out.push(p);
                }
            }
            None => out.push(p),
        }
    }
    for d in subdirs {
        collect_archives(&d, out);
    }
}

// ============================================================
// 规划阶段（打开 + 分类 + 两级去重，不写盘）
// ============================================================

struct PlannedTarget {
    entry: VirtualEntry,
    author: String,
    /// var 文件名（不含包内路径）——仓库内文件名与包内一致
    file_name: String,
}

struct ArchivePlan {
    display: String,
    /// 压缩包路径（密码候选重试时重开用）
    path: PathBuf,
    /// 当前打开所用密码（None = 无密码）
    pw: Option<String>,
    vfs: Option<Box<dyn ArchiveVFS>>,
    targets: Vec<PlannedTarget>,
}

#[derive(Default)]
struct ArchiveStat {
    new_files: usize,
    repo_exists: usize,
    in_archive_dup: usize,
    crc_conflict: usize,
    invalid: usize,
}

/// 密码候选序列：[无密码] + passwords（绝大多数包不加密，无密码优先零成本直通）。
/// 加密头包（7z/RAR）密码不符在 open 即失败——候选循环在阶段 A 推进；
/// 内容加密包（zip AES 等）open 恒成功、密码错误在解压时才暴露——候选循环在阶段 B 推进
fn password_candidates(passwords: &[String]) -> Vec<Option<String>> {
    std::iter::once(None)
        .chain(passwords.iter().cloned().map(Some))
        .collect()
}

/// 对单个包构建整理计划：分类 → 包内去重 → 仓库去重。
/// 返回待抽取目标（按包内顺序，solid 包 folder 定位成本最低）与统计。
fn build_plan(
    vfs: &dyn ArchiveVFS,
    display: &str,
    repo: &Path,
    lines: &mut Vec<ReportLine>,
) -> (Vec<PlannedTarget>, ArchiveStat) {
    let entries = vfs.list();
    let mut targets = Vec::new();
    let mut stat = ArchiveStat::default();
    // 包内去重状态：文件名 → 首见条目的 (size, crc)
    let mut seen: HashMap<String, (u64, Option<u32>)> = HashMap::new();

    for e in entries {
        if e.is_dir {
            continue;
        }
        // 兼容 '\' 分隔的包内路径（PowerShell Compress-Archive 等工具的产物）
        let base = e
            .name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&e.name)
            .to_string();
        match classify_file_name(&base) {
            ClassifyResult::Valid { author, .. } => {
                // ---- 包内去重（多级目录里的同名 var）----
                if let Some(&(size0, crc0)) = seen.get(&base) {
                    let same = size0 == e.size
                        && match (crc0, e.crc) {
                            (Some(a), Some(b)) => a == b,
                            // 无 CRC 的格式退化按尺寸判同
                            _ => true,
                        };
                    if same {
                        stat.in_archive_dup += 1;
                        lines.push(ReportLine::new(
                            LineKind::InArchiveDup,
                            display,
                            &e.name,
                            "与包内首见同名条目内容一致（CRC/尺寸相同），跳过",
                        ));
                    } else {
                        stat.crc_conflict += 1;
                        let crc_note = |c: Option<u32>| {
                            c.map(|v| format!("{v:08x}")).unwrap_or_else(|| "无".into())
                        };
                        lines.push(ReportLine::new(
                            LineKind::CrcConflict,
                            display,
                            &e.name,
                            format!(
                                "同名不同内容（CRC {} vs {}），已跳过，请人工确认",
                                crc_note(crc0),
                                crc_note(e.crc)
                            ),
                        ));
                    }
                    continue;
                }
                seen.insert(base.clone(), (e.size, e.crc));

                // ---- 仓库去重：仓库版胜出 ----
                let dest = repo.join(&author).join(&base);
                if long_path(&dest).exists() {
                    stat.repo_exists += 1;
                    lines.push(ReportLine::new(
                        LineKind::RepoExists,
                        display,
                        &e.name,
                        format!("{author}/{base}：仓库已有，保留仓库文件"),
                    ));
                } else {
                    stat.new_files += 1;
                    targets.push(PlannedTarget {
                        entry: e,
                        author,
                        file_name: base,
                    });
                }
            }
            ClassifyResult::Invalid(kind) => {
                stat.invalid += 1;
                lines.push(ReportLine::new(
                    LineKind::Invalid,
                    display,
                    &e.name,
                    format!("{}（不落盘，仍留在压缩包内）", kind.label()),
                ));
            }
        }
    }
    (targets, stat)
}

// ============================================================
// 执行阶段（抽取直写仓库）
// ============================================================

/// 原子改名且绝不覆盖已存在的目标（MoveFileExW 不带 REPLACE_EXISTING）。
/// 仓库红线（12.1）的最后一道闸：即使规划/竞态出现漏洞，也覆写不了仓库文件。
fn rename_no_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    };
    let s = wide(src);
    let d = wide(dst);
    let ok = unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// 超长路径保护：绝对路径且 ≥248 字符时加 `\\?\` 前缀（绕过 MAX_PATH）
fn long_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if p.is_absolute() && s.len() >= 248 && !s.starts_with("\\\\?\\") {
        PathBuf::from(format!("\\\\?\\{s}"))
    } else {
        p.to_path_buf()
    }
}

fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{mb:.1} MB")
    }
}

/// 单包执行：按计划批量抽取（staging 同目录 + 提升改名）。
/// - staging 文件名 `.vtp{pid}`：崩溃残留可安全手动删除（内容未提升，不是有效 var）
/// - file_done 回调里做「提升」：目标已存在 → 丢弃 staging 保留仓库文件（二次检查，
///   防规划后到落盘前仓库出现同名文件的竞态）；否则 rename_no_replace 落地
/// - 返回值：仅 `Err(Password)` 会向上传播（调用方推进密码候选重试）；
///   其余错误已在本函数内记录（summary/lines）并返回 Ok
fn execute_plan(
    plan: &mut ArchivePlan,
    repo: &Path,
    summary: &mut TidySummary,
    lines: &mut Vec<ReportLine>,
    out: &mut dyn FnMut(&str),
) -> Result<(), VfsError> {
    let Some(vfs) = plan.vfs.take() else {
        return Ok(());
    };
    let display = plan.display.clone();
    if plan.targets.is_empty() {
        return Ok(());
    }

    // 作者目录：create_dir_all 对已存在目录是 no-op，不触碰其中内容。
    // 记录本次新建的目录：密码候选耗尽时回收空目录，保持仓库零痕迹
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    let mut authors: Vec<String> = plan.targets.iter().map(|t| t.author.clone()).collect();
    authors.sort_unstable();
    authors.dedup();
    for a in &authors {
        let dir = long_path(&repo.join(a));
        if !dir.exists() {
            created_dirs.push(dir.clone());
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let note = format!("创建作者目录失败: {e}");
            let mut failed = 0usize;
            plan.targets.retain(|t| {
                if t.author == *a {
                    failed += 1;
                    lines.push(ReportLine::new(
                        LineKind::Failed,
                        &display,
                        &t.entry.name,
                        &note,
                    ));
                    false
                } else {
                    true
                }
            });
            summary.failed_files += failed;
        }
    }
    if plan.targets.is_empty() {
        return Ok(());
    }

    let pid = std::process::id();
    // (staging, final)：同目录 → 同卷 rename，原子且零拷贝
    let items: Vec<(PathBuf, PathBuf)> = plan
        .targets
        .iter()
        .map(|t| {
            let final_path = long_path(&repo.join(&t.author).join(&t.file_name));
            let staging = final_path.with_extension(format!("vtp{pid}"));
            (staging, final_path)
        })
        .collect();
    let targets: Vec<(&VirtualEntry, &Path)> = plan
        .targets
        .iter()
        .zip(items.iter())
        .map(|(t, (staging, _))| (&t.entry, staging.as_path()))
        .collect();

    // 进度/完成回调共享输出与统计（单线程，RefCell 无争用）。
    // summary/lines 也走 RefCell：extract_batch 的回调签名是 &dyn Fn（非 FnMut），
    // 闭包内可变状态必须经内部可变性传递
    let out_cell = std::cell::RefCell::new(out);
    let summary_cell = std::cell::RefCell::new(summary);
    let lines_cell = std::cell::RefCell::new(lines);
    let last_print = std::cell::RefCell::new(Instant::now() - PROGRESS_THROTTLE);
    let promoted = std::cell::Cell::new(0usize);
    let cancel = std::sync::atomic::AtomicBool::new(false);

    let progress = |done: u64, stage: &str| {
        {
            let mut last = last_print.borrow_mut();
            if last.elapsed() < PROGRESS_THROTTLE {
                return;
            }
            *last = Instant::now();
        }
        let mb = done / 1024 / 1024;
        let msg = if stage.is_empty() {
            format!("[{display}] 解压中… {mb} MB")
        } else {
            format!("[{display}] {stage} · {mb} MB")
        };
        (out_cell.borrow_mut())(&msg);
    };

    let file_done = |i: usize| {
        promoted.set(promoted.get() + 1);
        let (staging, final_path) = &items[i];
        let t = &plan.targets[i];
        if final_path.exists() {
            // 二次检查命中（规划后仓库出现了同名文件）：丢弃 staging，仓库版胜出
            std::fs::remove_file(staging).ok();
            summary_cell.borrow_mut().repo_exists += 1;
            lines_cell.borrow_mut().push(ReportLine::new(
                LineKind::RepoExists,
                &display,
                &t.entry.name,
                "执行时发现仓库已存在，保留仓库文件",
            ));
        } else if let Err(e) = rename_no_replace(staging, final_path) {
            std::fs::remove_file(staging).ok();
            summary_cell.borrow_mut().failed_files += 1;
            lines_cell.borrow_mut().push(ReportLine::new(
                LineKind::Failed,
                &display,
                &t.entry.name,
                format!("落盘失败: {e}"),
            ));
        } else {
            let mut s = summary_cell.borrow_mut();
            s.added += 1;
            s.added_bytes += t.entry.size;
            lines_cell.borrow_mut().push(ReportLine::new(
                LineKind::Added,
                &display,
                &t.entry.name,
                format!("{}/{} ({})", t.author, t.file_name, human_size(t.entry.size)),
            ));
            drop(s);
            (out_cell.borrow_mut())(&format!(
                "[{display}] 已新增 {}/{}",
                t.author, t.file_name
            ));
        }
    };

    match vfs.extract_batch(&targets, &cancel, &progress, &file_done) {
        Ok(()) => Ok(()),
        // 密码错误不在此记录：调用方可能换密码候选重试，避免中间状态进报告。
        // 回收本次空建的作者目录（remove_dir 仅删空目录，已有提升文件的目录保留）
        Err(e) if matches!(e, VfsError::Password) => {
            for (staging, _) in &items {
                std::fs::remove_file(staging).ok();
            }
            for d in &created_dirs {
                std::fs::remove_dir(d).ok();
            }
            Err(VfsError::Password)
        }
        Err(e) => {
            // 批量失败：清理残留 staging；未提升的条目记失败（逐条已完成的照常保留）
            for (staging, _) in &items {
                std::fs::remove_file(staging).ok();
            }
            let remaining = plan.targets.len() - promoted.get();
            let mut s = summary_cell.borrow_mut();
            s.failed_files += remaining;
            s.failed_archives += 1;
            drop(s);
            let note = format!("解压中止: {e}；其余 {remaining} 个文件未处理");
            lines_cell
                .borrow_mut()
                .push(ReportLine::new(LineKind::Failed, &display, "-", note));
            Ok(())
        }
    }
}

// ============================================================
// 主流程
// ============================================================

/// VarTidy 主流程：规划（全部包）→ 磁盘预检 → 逐包抽取 → 报告。
/// dry_run 只做规划与报告，不写任何文件。
pub fn run(
    inputs: &[PathBuf],
    opts: &TidyOptions,
    out: &mut dyn FnMut(&str),
) -> Result<TidyRun, String> {
    let archives = expand_inputs(inputs);
    if archives.is_empty() {
        return Err("未找到任何压缩包（支持 zip/7z/rar）".into());
    }
    let repo = &opts.repo_dir;
    if !repo.is_dir() {
        return Err(format!(
            "仓库目录不存在: {}（VarTidy 不创建仓库目录，请确认路径）",
            repo.display()
        ));
    }

    let mut summary = TidySummary::default();
    let mut lines: Vec<ReportLine> = Vec::new();
    let mut plans: Vec<ArchivePlan> = Vec::new();
    let candidates = password_candidates(&opts.passwords);

    // ---- 阶段 A：规划（打开 + 分类 + 两级去重；纯元数据，不解压不写盘）----
    for path in &archives {
        let display = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // 密码候选逐个尝试打开：加密头包（7z/RAR）密码不符在此即失败，换下一候选
        let mut opened: Option<(Box<dyn ArchiveVFS>, Option<String>)> = None;
        let mut open_err: Option<VfsError> = None;
        for cand in &candidates {
            match open_archive(path, cand.as_deref()) {
                Ok(vfs) => {
                    opened = Some((vfs, cand.clone()));
                    break;
                }
                Err(VfsError::Password) => continue,
                // 非密码错误（损坏/不存在）：换密码也没用，直接定性
                Err(e) => {
                    open_err = Some(e);
                    break;
                }
            }
        }
        match opened {
            Some((vfs, pw)) => {
                let (targets, stat) = build_plan(vfs.as_ref(), &display, repo, &mut lines);
                out(&format!(
                    "[{display}] 规划完成：新增 {} / 仓库已有 {} / 包内重复 {} / 同名不同内容 {} / 无效 {}",
                    stat.new_files, stat.repo_exists, stat.in_archive_dup, stat.crc_conflict, stat.invalid
                ));
                summary.archives += 1;
                summary.repo_exists += stat.repo_exists;
                summary.in_archive_dup += stat.in_archive_dup;
                summary.crc_conflict += stat.crc_conflict;
                summary.invalid += stat.invalid;
                plans.push(ArchivePlan {
                    display,
                    path: path.clone(),
                    pw,
                    vfs: Some(vfs),
                    targets,
                });
            }
            None => {
                summary.failed_archives += 1;
                let note = match open_err {
                    // 全部候选（含无密码）都报 Password = 加密头包且密码列表无匹配
                    None => "打开失败：加密包，所有候选密码均不匹配（可用 -p 或 config vartidy.passwords 补充）".to_string(),
                    Some(e) => format!("打开失败: {e}"),
                };
                lines.push(ReportLine::new(LineKind::Failed, &display, "-", note));
            }
        }
    }

    // ---- 阶段 B：抽取（dry_run 到此为止）----
    if !opts.dry_run {
        // 磁盘余量预检：全部计划新增量一次性核对（不足直接中止，未写入任何文件）
        let total_new: u64 = plans
            .iter()
            .flat_map(|p| p.targets.iter().map(|t| t.entry.size))
            .sum();
        if total_new > 0 {
            if let Some(free) = crate::cache::free_bytes(repo) {
                if free < total_new + DISK_HEADROOM_MB * 1024 * 1024 {
                    return Err(format!(
                        "仓库所在磁盘剩余 {}，本次需新增约 {}（另留 {} MB 余量），已中止，未写入任何文件",
                        human_size(free),
                        human_size(total_new),
                        DISK_HEADROOM_MB
                    ));
                }
            } else {
                out("警告：无法查询仓库磁盘剩余空间，跳过预检");
            }
        }

        for plan in &mut plans {
            // 密码候选重试循环：内容加密包（zip AES 等）open 成功但解压报 Password，
            // 推进到下一候选重开重试；已落盘文件按仓库存在性剔除，不重复落盘与计数
            let mut cand_idx = candidates.iter().position(|c| *c == plan.pw).unwrap_or(0);
            loop {
                match execute_plan(plan, repo, &mut summary, &mut lines, out) {
                    Ok(()) => break,
                    // 仅 Password 会传播到此（其余错误已在 execute_plan 内记录）
                    Err(VfsError::Password) => {
                        let mut next: Option<(Box<dyn ArchiveVFS>, Option<String>)> = None;
                        while let Some(cand) = candidates.get(cand_idx + 1) {
                            cand_idx += 1;
                            match open_archive(&plan.path, cand.as_deref()) {
                                Ok(vfs) => {
                                    next = Some((vfs, cand.clone()));
                                    break;
                                }
                                Err(VfsError::Password) => continue,
                                Err(e) => {
                                    summary.failed_archives += 1;
                                    lines.push(ReportLine::new(
                                        LineKind::Failed,
                                        &plan.display,
                                        "-",
                                        format!("重开失败: {e}"),
                                    ));
                                    break;
                                }
                            }
                        }
                        match next {
                            Some((vfs, pw)) => {
                                plan.vfs = Some(vfs);
                                plan.pw = pw;
                                let display = plan.display.clone();
                                let keep: Vec<PlannedTarget> = plan
                                    .targets
                                    .drain(..)
                                    .filter(|t| {
                                        !long_path(&repo.join(&t.author).join(&t.file_name))
                                            .exists()
                                    })
                                    .collect();
                                plan.targets = keep;
                                out(&format!("[{display}] 切换密码候选，继续解压剩余条目"));
                            }
                            None => {
                                summary.failed_archives += 1;
                                lines.push(ReportLine::new(
                                    LineKind::Failed,
                                    &plan.display,
                                    "-",
                                    "解压失败：加密内容，所有候选密码均不匹配（可用 -p 或 config vartidy.passwords 补充）",
                                ));
                                break;
                            }
                        }
                    }
                    // 防御分支：非密码错误不应到达（execute_plan 内部已记录）
                    Err(e) => {
                        summary.failed_archives += 1;
                        lines.push(ReportLine::new(
                            LineKind::Failed,
                            &plan.display,
                            "-",
                            format!("解压失败: {e}"),
                        ));
                        break;
                    }
                }
            }
        }
    }

    // ---- 报告 ----
    let report_path = write_report(inputs, opts, &summary, &lines, out);
    Ok(TidyRun {
        summary,
        report_path,
    })
}

// ============================================================
// 报告
// ============================================================

/// 本地时间字符串（无 chrono 依赖，直接问系统）
fn local_time(fmt_date: fn(u16, u16, u16, u16, u16, u16) -> String) -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t = unsafe { std::mem::zeroed::<SYSTEMTIME>() };
    unsafe { GetLocalTime(&mut t) };
    fmt_date(t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn timestamp_for_report() -> (String, String) {
    (
        local_time(|y, mo, d, h, mi, s| format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")),
        local_time(|y, mo, d, h, mi, s| format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}")),
    )
}

/// 报告目录：显式配置优先；否则首个输入所在目录（输入为目录时用该目录本身）。
/// 报告不写入仓库——仓库只进 var 与作者目录，保持纯净
fn default_report_dir(inputs: &[PathBuf]) -> PathBuf {
    inputs
        .first()
        .map(|p| {
            if p.is_dir() {
                p.clone()
            } else {
                p.parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(|d| d.to_path_buf())
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    })
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn write_report(
    inputs: &[PathBuf],
    opts: &TidyOptions,
    summary: &TidySummary,
    lines: &[ReportLine],
    out: &mut dyn FnMut(&str),
) -> Option<PathBuf> {
    let dir = match &opts.report_dir {
        Some(d) => d.clone(),
        None => default_report_dir(inputs),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        out(&format!("警告：报告目录创建失败 [{}]：{e}", dir.display()));
        return None;
    }
    let (now, compact) = timestamp_for_report();
    let path = dir.join(format!("vartidy_report_{compact}.txt"));
    let text = build_report_text(&now, opts, summary, lines);
    if let Err(e) = std::fs::write(&path, text) {
        out(&format!("警告：报告写入失败 [{}]：{e}", path.display()));
        return None;
    }
    Some(path)
}

fn build_report_text(
    now: &str,
    opts: &TidyOptions,
    summary: &TidySummary,
    lines: &[ReportLine],
) -> String {
    let mut s = String::new();
    s.push_str("==================== VarTidy 整理报告 ====================\n");
    s.push_str(&format!("时间   : {now}\n"));
    s.push_str(&format!(
        "模式   : {}\n",
        if opts.dry_run { "预演 (dry-run，未写入任何文件)" } else { "实际执行" }
    ));
    s.push_str(&format!("仓库   : {}\n", opts.repo_dir.display()));
    s.push_str(&format!("输入   : {} 个压缩包\n", summary.archives + summary.failed_archives));
    s.push_str("\n---------------------- 汇总 ----------------------\n");
    s.push_str(&format!(
        "新增文件           : {} 个 ({})\n",
        summary.added,
        human_size(summary.added_bytes)
    ));
    s.push_str(&format!("仓库已有(跳过)     : {} 个\n", summary.repo_exists));
    s.push_str(&format!("包内重复(跳过)     : {} 个\n", summary.in_archive_dup));
    s.push_str(&format!(
        "同名不同内容(警告) : {} 个  ← 内容与首见条目不同，已跳过，需人工确认\n",
        summary.crc_conflict
    ));
    s.push_str(&format!(
        "无效条目           : {} 个  ← 未落盘，仍留在压缩包内\n",
        summary.invalid
    ));
    s.push_str(&format!(
        "失败               : 文件 {} 个 / 压缩包 {} 个\n",
        summary.failed_files, summary.failed_archives
    ));

    for kind in [
        LineKind::Added,
        LineKind::RepoExists,
        LineKind::InArchiveDup,
        LineKind::CrcConflict,
        LineKind::Invalid,
        LineKind::Failed,
    ] {
        let section: Vec<&ReportLine> = lines.iter().filter(|l| l.kind == kind).collect();
        if section.is_empty() {
            continue;
        }
        s.push_str(&format!("\n------------------ 明细：{} ------------------\n", kind.label()));
        for l in section {
            s.push_str(&format!("[{}] {} ： {}\n", l.archive, l.entry, l.note));
        }
    }
    s
}

// ============================================================
// CLI 入口
// ============================================================

const USAGE: &str = "\
archview tidy — var 资源包增量整理（架构与规则见 ARCHITECTURE.md 第 12 章）

用法: archview tidy <压缩包或目录> [更多输入...] [选项]

输入: 压缩包（zip/7z/rar，支持分卷 RAR）或目录（递归扫描其中的压缩包）

选项:
  -o, --repo <dir>     var 仓库目录（缺省用 config.toml 的 vartidy.repo_dir）
                       仓库已有文件绝不会被修改/覆盖/删除，仅增量合入新 var
  -n, --dry-run        预演：只做分类与两级去重检查并输出报告，不写任何文件
  -p, --password <pw>  加密压缩包密码（可多次指定，按序尝试；
                       config.toml 的 vartidy.passwords 密码列表也会按序兜底尝试）
  -q, --quiet          静默：无控制台时结束也不弹结果窗（脚本/计划任务场景防阻塞；
                       有控制台时输出照常）
      --report <dir>   报告输出目录（缺省: 首个输入所在目录，不写入仓库）
  -h, --help           显示本帮助

示例:
  archview tidy D:/downloads/pack.7z -o D:/var_repo
  archview tidy D:/downloads -n        # 先预演看报告，确认后再实际执行

「发送到」快捷方式: 目标填 \"<archview.exe 完整路径>\" tidy \"%1\"
（仓库目录在 config.toml 的 vartidy.repo_dir 配置，无需命令行参数）";

/// CLI 输出：GUI 子系统 exe 从终端启动无控制台——附加父进程控制台经 CONOUT$ 直写；
/// 「发送到」/双击启动（附加失败）→ 输出全走报告文件，结束时弹窗汇总
struct Out {
    con: Option<std::fs::File>,
    had_console: bool,
}

impl Out {
    fn attach() -> Self {
        unsafe {
            let has = !windows_sys::Win32::System::Console::GetConsoleWindow().is_null();
            if has {
                return Self {
                    con: None,
                    had_console: true,
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
            }
        }
    }

    fn line(&self, s: &str) {
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
}

fn msgbox(text: &str, error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    };
    let wide = |s: &str| -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    };
    let t = wide(text);
    let c = wide("Archview VarTidy");
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | icon);
    }
}

struct ParsedArgs {
    inputs: Vec<String>,
    repo: Option<String>,
    dry_run: bool,
    passwords: Vec<String>,
    quiet: bool,
    report: Option<String>,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut p = ParsedArgs {
        inputs: Vec::new(),
        repo: None,
        dry_run: false,
        passwords: Vec::new(),
        quiet: false,
        report: None,
        help: false,
    };
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => p.help = true,
            "-n" | "--dry-run" => p.dry_run = true,
            "-q" | "--quiet" => p.quiet = true,
            "-o" | "--repo" => {
                i += 1;
                p.repo = Some(args.get(i).ok_or("--repo 需要一个目录参数")?.clone());
            }
            "-p" | "--password" => {
                i += 1;
                p.passwords
                    .push(args.get(i).ok_or("--password 需要一个密码参数")?.clone());
            }
            "--report" => {
                i += 1;
                p.report = Some(args.get(i).ok_or("--report 需要一个目录参数")?.clone());
            }
            s if s.starts_with('-') => return Err(format!("未知选项: {s}")),
            s => p.inputs.push(s.to_string()),
        }
        i += 1;
    }
    Ok(p)
}

/// 密码列表合并：CLI -p 在前（显式指定优先尝试），config vartidy.passwords 在后；
/// 去重保序、剔除空串
fn merge_passwords(cli: &[String], cfg: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in cli.iter().chain(cfg.iter()) {
        if !p.is_empty() && !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// CLI 主入口（main.rs 按 `tidy` 子命令分发）。
/// 返回进程退出码：0 = 完成；1 = 有失败项；2 = 用法/配置错误
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
        out.line(&format!("缺少输入（至少一个压缩包或目录）\n\n{USAGE}"));
        if !quiet {
            msgbox("缺少输入：请在命令后指定压缩包或目录，或用「发送到」发送压缩包。", true);
        }
        return 2;
    }

    let cfg = match crate::config::Config::load_or_create() {
        Ok(c) => c,
        Err(e) => {
            out.line(&format!("config.toml 加载失败: {e}"));
            if !quiet {
                msgbox(&format!("config.toml 加载失败: {e}"), true);
            }
            return 2;
        }
    };
    crate::vfs::set_password_codepage(&cfg.password_codepage);

    // 仓库目录：-o 优先，缺省 config.toml vartidy.repo_dir
    let repo = match parsed.repo.clone().or_else(|| {
        let r = cfg.vartidy.repo_dir.trim().to_string();
        (!r.is_empty()).then_some(r)
    }) {
        Some(r) => PathBuf::from(r),
        None => {
            let msg = "未指定仓库目录：用 -o <dir> 或在 config.toml 的 vartidy.repo_dir 配置";
            out.line(msg);
            if !quiet {
                msgbox(msg, true);
            }
            return 2;
        }
    };
    // 报告目录：--report 优先，config "auto"/空 = 自动（首个输入所在目录）
    let report_dir = match parsed.report {
        Some(r) => Some(PathBuf::from(r)),
        None => {
            let r = cfg.vartidy.report_dir.trim();
            (!r.is_empty() && r != "auto").then(|| PathBuf::from(r))
        }
    };

    let inputs: Vec<PathBuf> = parsed.inputs.iter().map(PathBuf::from).collect();
    let opts = TidyOptions {
        repo_dir: repo,
        dry_run: parsed.dry_run,
        report_dir,
        // CLI -p 优先，config vartidy.passwords 兜底
        passwords: merge_passwords(&parsed.passwords, &cfg.vartidy.passwords),
    };

    let mut printer = |s: &str| out.line(s);
    match run(&inputs, &opts, &mut printer) {
        Ok(r) => {
            let s = &r.summary;
            let summary_text = format!(
                "{}：新增 {} 个 ({}) / 仓库已有 {} / 包内重复 {} / 同名不同内容 {} / 无效 {} / 失败 文件{}+包{}",
                if opts.dry_run { "预演完成" } else { "整理完成" },
                s.added,
                human_size(s.added_bytes),
                s.repo_exists,
                s.in_archive_dup,
                s.crc_conflict,
                s.invalid,
                s.failed_files,
                s.failed_archives,
            );
            out.line(&summary_text);
            if let Some(rp) = &r.report_path {
                out.line(&format!("报告: {}", rp.display()));
            }
            // 无控制台（「发送到」/双击）且非静默：弹窗呈现结果
            if !out.had_console && !quiet {
                let report_note = r
                    .report_path
                    .as_ref()
                    .map(|p| format!("\n\n报告文件:\n{}", p.display()))
                    .unwrap_or_default();
                let conflict_note = if s.crc_conflict > 0 {
                    format!("\n\n注意: 有 {} 个同名不同内容的 var 被跳过，详见报告。", s.crc_conflict)
                } else {
                    String::new()
                };
                msgbox(&format!("{summary_text}{conflict_note}{report_note}"), false);
            }
            if s.failed_files > 0 || s.failed_archives > 0 {
                1
            } else {
                0
            }
        }
        Err(e) => {
            out.line(&format!("整理失败: {e}"));
            if !out.had_console && !quiet {
                msgbox(&format!("整理失败: {e}"), true);
            }
            1
        }
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("vartidy_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造 zip 测试包：条目 (包内路径, 内容)
    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut w = zip::write::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    /// 造 7z 测试包（每文件独立 folder，非 solid）
    fn make_7z(dir: &Path, path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = sevenz_rust::SevenZWriter::create(path).unwrap();
        for (name, data) in entries {
            let payload = dir.join(format!("payload_{}", name.replace('/', "_")));
            std::fs::write(&payload, data).unwrap();
            let e = sevenz_rust::SevenZArchiveEntry::from_path(&payload, (*name).into());
            w.push_archive_entry(e, Some(std::fs::File::open(&payload).unwrap()))
                .unwrap();
        }
        w.finish().unwrap();
    }

    fn run_quiet(inputs: &[PathBuf], opts: &TidyOptions) -> TidyRun {
        run(inputs, opts, &mut |_| {}).unwrap()
    }

    /// ZIP 全链路：分类 / 包内去重（同 CRC 与不同 CRC）/ 仓库去重（仓库版胜出）/
    /// dry-run 零写入 / 实际执行落地 / 幂等重跑
    #[test]
    fn zip_end_to_end() {
        let dir = temp_dir("zip");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("Bob")).unwrap();
        // 仓库已有 Bob/Bob.scene.2.var（旧内容）——绝不允许被覆盖。
        // 注意约定：文件进作者目录保留完整原名（与 class_var.py 一致）
        std::fs::write(repo.join("Bob").join("Bob.scene.2.var"), b"OLD-REPO-VERSION").unwrap();

        let content_x = vec![0x58u8; 4096]; // Alice/look1.1.var 内容
        let content_y1 = vec![1u8; 2048]; // Bob/scene.2.var 包内第一份
        let content_y2 = vec![2u8; 2048]; // Bob/scene.2.var 包内第二份（CRC 不同）
        let arch = dir.join("pack.zip");
        make_zip(
            &arch,
            &[
                ("dir1/Alice.look1.1.var", &content_x),
                ("dir2/Alice.look1.1.var", &content_x), // 包内重复（同内容同 CRC）
                ("dir3/Bob.scene.2.var", &content_y1),
                ("dir4/Bob.scene.2.var", &content_y2), // 包内同名不同内容
                ("dir1/readme.txt", b"not a var"),
                ("dir1/Alice.bad.var", b"x"),           // 三段 → 格式错误
                ("dir1/Alice.item.x.var", b"x"),        // 版本号非数字
            ],
        );

        let opts = |dry: bool| TidyOptions {
            repo_dir: repo.clone(),
            dry_run: dry,
            report_dir: Some(dir.clone()),
            passwords: vec![],
        };

        // ---- dry-run：不写任何文件 ----
        let r = run_quiet(&[arch.clone()], &opts(true));
        assert_eq!(r.summary.crc_conflict, 1);
        assert_eq!(r.summary.in_archive_dup, 1);
        assert_eq!(r.summary.repo_exists, 1, "仓库已有的 Bob/scene.2.var 应跳过");
        assert_eq!(r.summary.invalid, 3, "readme.txt + 格式错误 + 版本号错误");
        // 计划新增 = Alice 一份
        assert_eq!(r.summary.added, 0, "dry-run 不落盘");
        assert!(!repo.join("Alice").exists(), "dry-run 不得创建作者目录");
        assert!(r.report_path.is_some(), "dry-run 也应产出报告");

        // ---- 实际执行 ----
        let r = run_quiet(&[arch.clone()], &opts(false));
        assert_eq!(r.summary.added, 1);
        assert_eq!(r.summary.added_bytes, content_x.len() as u64);
        // Alice 新增且内容一致（完整文件名进作者目录）
        let written = std::fs::read(repo.join("Alice").join("Alice.look1.1.var")).unwrap();
        assert_eq!(written, content_x);
        // 仓库版胜出：Bob/Bob.scene.2.var 仍是旧内容
        let bob = std::fs::read(repo.join("Bob").join("Bob.scene.2.var")).unwrap();
        assert_eq!(bob, b"OLD-REPO-VERSION");
        // 仓库内无垃圾（无效文件不落盘）
        let bob_files: Vec<_> = std::fs::read_dir(repo.join("Bob")).unwrap().collect();
        assert_eq!(bob_files.len(), 1);
        let alice_files: Vec<_> = std::fs::read_dir(repo.join("Alice")).unwrap().collect();
        assert_eq!(alice_files.len(), 1);

        // ---- 幂等重跑：全部命中仓库已有，零新增 ----
        let r = run_quiet(&[arch.clone()], &opts(false));
        assert_eq!(r.summary.added, 0);
        assert_eq!(r.summary.repo_exists, 2, "Alice/Bob 两份均已有");
        let bob = std::fs::read(repo.join("Bob").join("Bob.scene.2.var")).unwrap();
        assert_eq!(bob, b"OLD-REPO-VERSION", "重跑仍不得动仓库文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 7z 全链路：作者目录创建 / 落盘内容 / 无效条目不落盘 / 幂等
    #[test]
    fn sevenz_end_to_end() {
        let dir = temp_dir("7z");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let c1 = vec![0xA1u8; 8192];
        let c2 = vec![0xB2u8; 4096];
        let arch = dir.join("pack.7z");
        make_7z(
            &dir,
            &arch,
            &[
                ("Cara.hair.3.var", &c1),
                ("sub/Deep.Dress.x.1.var", &c2),
                ("note.txt", b"junk"),
            ],
        );

        let opts = TidyOptions {
            repo_dir: repo.clone(),
            dry_run: false,
            report_dir: Some(dir.clone()),
            passwords: vec![],
        };
        let r = run_quiet(&[arch.clone()], &opts);
        assert_eq!(r.summary.added, 2);
        assert_eq!(r.summary.invalid, 1);
        // 完整文件名进作者目录（Cara/Cara.hair.3.var）
        assert_eq!(
            std::fs::read(repo.join("Cara").join("Cara.hair.3.var")).unwrap(),
            c1
        );
        // 多级子目录 → 平铺到作者目录
        assert_eq!(
            std::fs::read(repo.join("Deep").join("Deep.Dress.x.1.var")).unwrap(),
            c2
        );
        assert!(!repo.join("sub").exists(), "包内目录结构不应带入仓库");

        // 幂等
        let r = run_quiet(&[arch], &opts);
        assert_eq!(r.summary.added, 0);
        assert_eq!(r.summary.repo_exists, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 输入展开：目录递归扫描 + 分卷 RAR 族去重 + 非压缩包排除
    #[test]
    fn expand_inputs_recursive_and_rar_family() {
        let dir = temp_dir("expand");
        std::fs::write(dir.join("a.zip"), [0u8; 8]).unwrap();
        std::fs::write(dir.join("note.txt"), b"x").unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("b.7z"), [0u8; 8]).unwrap();
        std::fs::write(dir.join("x.part1.rar"), [0u8; 8]).unwrap();
        std::fs::write(dir.join("x.part2.rar"), [0u8; 8]).unwrap();
        std::fs::write(dir.join("x.part10.rar"), [0u8; 8]).unwrap();

        let mut got: Vec<String> = expand_inputs(&[dir.clone()])
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, ["a.zip", "b.7z", "x.part1.rar"]);

        // 单文件输入：非压缩包被忽略 → run 阶段报"未找到"由调用方处理
        assert!(expand_inputs(&[dir.join("note.txt")]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rename_no_replace：目标已存在必须失败（仓库红线的最后一道闸）
    #[test]
    fn rename_never_overwrites() {
        let dir = temp_dir("rename");
        let a = dir.join("a.var");
        let b = dir.join("b.var");
        let c = dir.join("c.var");
        std::fs::write(&a, b"AAA").unwrap();
        std::fs::write(&b, b"BBB").unwrap();

        assert!(rename_no_replace(&a, &b).is_err(), "目标已存在时必须失败");
        assert_eq!(std::fs::read(&b).unwrap(), b"BBB", "已有文件内容不得被改动");
        assert!(std::fs::read(&a).is_ok(), "源文件应保留");

        rename_no_replace(&a, &c).unwrap();
        assert_eq!(std::fs::read(&c).unwrap(), b"AAA");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 参数解析（-p 可多次）
    #[test]
    fn cli_args_parsing() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let p = parse_args(&args(&["D:/dl", "-n", "-o", "D:/repo", "-p", "pw1", "-p", "pw2", "x.zip"])).unwrap();
        assert_eq!(p.inputs, ["D:/dl", "x.zip"]);
        assert!(p.dry_run);
        assert_eq!(p.repo.as_deref(), Some("D:/repo"));
        assert_eq!(p.passwords, ["pw1", "pw2"]);
        assert!(parse_args(&args(&["--bogus"])).is_err());
        assert!(parse_args(&args(&["-o"])).is_err(), "缺参数值应报错");
    }

    /// 密码列表合并：CLI 优先、config 兜底、去重保序、剔除空串
    #[test]
    fn password_merge_order_dedup() {
        let cli = ["a".to_string(), "".to_string()];
        let cfg = ["b".to_string(), "a".to_string()];
        assert_eq!(merge_passwords(&cli, &cfg), ["a", "b"]);
        assert!(merge_passwords(&[], &[]).is_empty());
    }

    /// 加密包密码候选链路：
    /// - zip AES（内容加密）：open 恒成功，密码错误在解压期暴露 → 阶段 B 推进候选重试
    /// - 7z 加密头：密码错误在 open 即失败 → 阶段 A 推进候选
    /// - 候选全错：两个包记密码失败，仓库零写入
    #[test]
    fn encrypted_password_candidates() {
        let dir = temp_dir("enc");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // AES zip（内容加密，文件名明文可见）
        let zpath = dir.join("enc.zip");
        {
            let file = std::fs::File::create(&zpath).unwrap();
            let mut w = zip::write::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .with_aes_encryption(zip::AesMode::Aes256, "right-pw");
            w.start_file("dir/Enc.look1.1.var", opts).unwrap();
            w.write_all(b"zip encrypted payload").unwrap();
            w.finish().unwrap();
        }
        // 7z 加密头（open 阶段即验密码）
        let spath = dir.join("enc_head.7z");
        {
            let payload = dir.join("pl.bin");
            std::fs::write(&payload, b"7z encrypted payload").unwrap();
            let mut w = sevenz_rust::SevenZWriter::create(&spath).unwrap();
            w.set_encrypt_header(true);
            w.set_content_methods(vec![sevenz_rust::AesEncoderOptions::new(
                sevenz_rust::Password::from("right-pw"),
            )
            .into()]);
            let e = sevenz_rust::SevenZArchiveEntry::from_path(&payload, "Enc.two.2.var".into());
            w.push_archive_entry(e, Some(std::fs::File::open(&payload).unwrap()))
                .unwrap();
            w.finish().unwrap();
        }

        // 候选序列 [无密码, 错, 对]：zip 走解压期重试，7z 走打开期推进，最终都命中
        let opts = TidyOptions {
            repo_dir: repo.clone(),
            dry_run: false,
            report_dir: Some(dir.clone()),
            passwords: vec!["wrong-pw".into(), "right-pw".into()],
        };
        let r = run_quiet(&[zpath.clone(), spath.clone()], &opts);
        assert_eq!(r.summary.failed_archives, 0, "两个加密包都应解出");
        assert_eq!(r.summary.added, 2);
        assert_eq!(
            std::fs::read(repo.join("Enc").join("Enc.look1.1.var")).unwrap(),
            b"zip encrypted payload"
        );
        assert_eq!(
            std::fs::read(repo.join("Enc").join("Enc.two.2.var")).unwrap(),
            b"7z encrypted payload"
        );

        // 候选全错（新仓库基线）：zip 在解压期耗尽、7z 在打开期耗尽，均记失败且零写入
        let repo2 = dir.join("repo2");
        std::fs::create_dir_all(&repo2).unwrap();
        let opts_bad = TidyOptions {
            repo_dir: repo2.clone(),
            passwords: vec!["wrong-pw".into()],
            ..opts
        };
        let r = run_quiet(&[zpath, spath], &opts_bad);
        assert_eq!(r.summary.added, 0);
        assert_eq!(r.summary.failed_archives, 2);
        assert!(
            repo2.read_dir().unwrap().next().is_none(),
            "密码失败路径不得写仓库"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 中文与特殊字符密码：
    /// - AES zip 中文密码（zip crate 以 UTF-8 字节加密；VFS 层候选含 UTF-8，直通）
    /// - 7z 混合密码（中文 + 特殊字符；7z 规范 UTF-16LE，sevenz-rust 内部转换）
    /// - CLI 参数解析对特殊字符密码不变形
    #[test]
    fn encrypted_non_ascii_and_special_passwords() {
        let dir = temp_dir("enc_cjk");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // AES zip，中文密码
        let zh_pw = "中文密码测试2026";
        let zpath = dir.join("enc_zh.zip");
        {
            let file = std::fs::File::create(&zpath).unwrap();
            let mut w = zip::write::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .with_aes_encryption(zip::AesMode::Aes256, zh_pw);
            w.start_file("Zh/中文名.look1.1.var", opts).unwrap();
            w.write_all(b"zh password payload").unwrap();
            w.finish().unwrap();
        }
        // 7z 内容加密，中文 + 特殊字符混合密码
        let sp_pw = "特!@#$%^&*()殊_密码";
        let spath = dir.join("enc_sp.7z");
        {
            let payload = dir.join("pl.bin");
            std::fs::write(&payload, b"special password payload").unwrap();
            let mut w = sevenz_rust::SevenZWriter::create(&spath).unwrap();
            w.set_content_methods(vec![sevenz_rust::AesEncoderOptions::new(
                sevenz_rust::Password::from(sp_pw),
            )
            .into()]);
            let e = sevenz_rust::SevenZArchiveEntry::from_path(&payload, "Sp.item.1.var".into());
            w.push_archive_entry(e, Some(std::fs::File::open(&payload).unwrap()))
                .unwrap();
            w.finish().unwrap();
        }

        // 密码列表含正确密码（前面垫一个错误候选，验证推进）
        let opts = TidyOptions {
            repo_dir: repo.clone(),
            dry_run: false,
            report_dir: Some(dir.clone()),
            passwords: vec!["错误候选".into(), zh_pw.into(), sp_pw.into()],
        };
        let r = run_quiet(&[zpath, spath], &opts);
        assert_eq!(r.summary.failed_archives, 0, "中文/特殊字符密码包都应解出");
        assert_eq!(r.summary.added, 2);
        assert_eq!(
            // 作者名取自 var 文件名（中文名.look1.1.var → 作者"中文名"），包内目录不参与
            std::fs::read(repo.join("中文名").join("中文名.look1.1.var")).unwrap(),
            b"zh password payload"
        );
        assert_eq!(
            std::fs::read(repo.join("Sp").join("Sp.item.1.var")).unwrap(),
            b"special password payload"
        );

        // CLI 解析：特殊字符密码原样保留
        let args: Vec<String> = ["-p", "p@ss #!$%^&*", "x.zip"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = parse_args(&args).unwrap();
        assert_eq!(p.passwords, ["p@ss #!$%^&*"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 作者名与包名含中文/空格/特殊字符（与 class_var.py 行为对齐：
    /// 作者 = 按点切分的第一段 strip，段内字符原样保留——空格/中文/& 等不影响分类；
    /// 包文件名仅作路径处理，PathBuf 原生 Unicode）
    #[test]
    fn author_and_archive_names_with_special_chars() {
        let dir = temp_dir("special_names");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // 包文件名带中文与空格
        let arch = dir.join("我的 var 包 2026.zip");
        let content = vec![0x33u8; 512];
        make_zip(
            &arch,
            &[
                // 作者带空格 + 特殊字符
                ("d1/Author Name.look1.1.var", &content),
                // 中文作者 + 中文内容
                ("d2/作者甲.中文内容.2.var", &content),
                // 特殊字符全家桶（Windows 文件名合法字符）
                ("d3/A&B[C]#100%.scene.3.var", &content),
                // 包内目录也带中文与空格
                ("子 目录/中文 作者.内容.1.var", &content),
            ],
        );

        let opts = TidyOptions {
            repo_dir: repo.clone(),
            dry_run: false,
            report_dir: Some(dir.clone()),
            passwords: vec![],
        };
        let r = run_quiet(&[arch], &opts);
        assert_eq!(r.summary.invalid, 0, "特殊字符作者名全部应判合法");
        assert_eq!(r.summary.added, 4);
        for rel in [
            "Author Name/Author Name.look1.1.var",
            "作者甲/作者甲.中文内容.2.var",
            "A&B[C]#100%/A&B[C]#100%.scene.3.var",
            "中文 作者/中文 作者.内容.1.var",
        ] {
            let p = repo.join(rel);
            assert!(
                p.exists(),
                "应存在: {rel}（实际 {:?}）",
                p.display().to_string()
            );
            assert_eq!(std::fs::read(&p).unwrap(), content);
        }

        // 幂等重跑
        let r = run_quiet(&[dir.join("我的 var 包 2026.zip")], &opts);
        assert_eq!(r.summary.added, 0);
        assert_eq!(r.summary.repo_exists, 4);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
