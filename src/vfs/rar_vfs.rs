//! RarVFS：RAR 格式（unrar crate 内嵌编译 UnRAR 源码，支持 RAR4/5、分卷）。
//! solid 包（`rar a -s`，主头 ROADF_SOLID）走后台解压会话 + 滑窗预取；
//! 非 solid 包（rar 默认）逐文件独立压缩，随机访问只解点击项（单文件按需）。

use super::{
    AccessMode, ArchiveVFS, ExtractPhase, SharedSession, VfsError, VirtualEntry, VirtualFile,
};
use std::path::{Path, PathBuf};
use unrar::Archive;

pub struct RarVFS {
    path: PathBuf,
    /// 解密密码候选（UTF-8/GBK 等，见 vfs::password_candidates）；空 = 未提供密码。
    /// 加密头包在 open 阶段就会解析出唯一正确候选（见 open）。
    password: Vec<Vec<u8>>,
    /// 包是否 solid（RAR 主头 ROADF_SOLID 标志）。决定 access_mode：
    /// solid = Solid（解压会话滑窗），非 solid = SingleFile（只解点击项）
    solid: bool,
}

/// unrar 错误信息按 Display 文本判断密码问题（"Password ... not specified"/"Wrong password"）
fn is_password_err(e: &unrar::error::UnrarError) -> bool {
    e.to_string().to_lowercase().contains("password")
}

impl RarVFS {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self, VfsError> {
        // 分卷：打开第一个卷
        let first = Archive::new(path).first_part();
        if !Archive::new(&first).is_archive() {
            return Err(VfsError::Archive("不是有效的 RAR 文件".into()));
        }
        let mut candidates = password.map(super::password_candidates).unwrap_or_default();

        // 校验能否打开（加密头在无/错密码时在此暴露）。
        // 探测 = listing + 读首个头：加密头的解密失败在 read_header 时报出
        let try_list = |pw: Option<&[u8]>| -> Result<(), unrar::error::UnrarError> {
            let arch = match pw {
                Some(p) => Archive::with_password(&first, p),
                None => Archive::new(&first),
            };
            let open = arch.open_for_listing()?;
            match open.read_header() {
                Ok(_) => Ok(()), // Ok(Some) 或 Ok(None)（空包）
                Err(e) => Err(e),
            }
        };
        match try_list(None) {
            // 头未加密（或包无加密）：listing 无需密码，候选原样保留到解压时再逐个试
            Ok(()) => {}
            Err(e) if is_password_err(&e) => {
                // 加密头：候选逐个试 listing，成功的解析为唯一密码
                if candidates.is_empty() {
                    return Err(VfsError::Password);
                }
                match candidates
                    .iter()
                    .find(|c| try_list(Some(c.as_slice())).is_ok())
                {
                    Some(pw) => candidates = vec![pw.clone()],
                    None => return Err(VfsError::Password),
                }
            }
            Err(e) => return Err(VfsError::Archive(format!("打开 RAR 失败: {e}"))),
        }
        // solid 判定：主头 ROADF_SOLID 标志（`rar a -s`），在 open 阶段零成本读出
        let solid = Self::detect_solid(&first, candidates.first().map(|p| p.as_slice()));
        Ok(Self {
            path: first.to_path_buf(),
            password: candidates,
            solid,
        })
    }

    /// 包是否 solid：读 RAR 主头标志（`unrar::OpenArchive::is_solid`）。
    /// 探测失败（极端：加密头候选全部失效——上面已拦，正常到不了）按 solid
    /// 兜底走慢路径，宁可多预取不可错判非 solid
    fn detect_solid(first: &Path, pw: Option<&[u8]>) -> bool {
        let arch = match pw {
            Some(p) => Archive::with_password(first, p),
            None => Archive::new(first),
        };
        arch.open_for_listing()
            .map(|o| o.is_solid())
            .unwrap_or(true)
    }

    /// listing 用（头未加密时任何候选/无密码均可）
    fn archive(&self) -> Archive<'_> {
        match self.password.first() {
            Some(pw) => Archive::with_password(&self.path, pw.as_slice()),
            None => Archive::new(&self.path),
        }
    }

    /// 单个密码候选的批量解压：一次 processing 遍历顺带解出全部目标
    /// （solid RAR 定位目标需逐条 skip，一次遍历解一批摊薄固定成本）。
    /// 无 list 下标可用（静态批不知道批内目标在包内的物理次序），
    /// 分段退化：非目标 skip 报 SkipUp（无定比总量，UI 走活动指示）、
    /// 目标解压报 Target（unrar 无逐字节回调，写前 0、完成后全量）
    fn try_batch(
        &self,
        pw: Option<&[u8]>,
        targets: &[(&VirtualEntry, &Path)],
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
        file_done: &dyn Fn(usize),
    ) -> Result<(), VfsError> {
        use std::collections::HashMap;
        let arch = match pw {
            Some(p) => Archive::with_password(&self.path, p),
            None => Archive::new(&self.path),
        };
        let mut open = arch.open_for_processing().map_err(|e| {
            VfsError::Archive(format!("打开 RAR 失败 [{}]: {e}", self.path.display()))
        })?;
        let mut remaining: HashMap<String, usize> = targets
            .iter()
            .enumerate()
            .map(|(i, (e, _))| (e.name.clone(), i))
            .collect();
        let mut skipped: usize = 0;
        let mut skipped_bytes: u64 = 0;
        loop {
            // 逐条目定位期间检查取消（大包 / NAS 上定位耗时分钟级）
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(VfsError::Cancelled);
            }
            match open.read_header().map_err(|e| {
                VfsError::Archive(format!("读 RAR 头失败 [{}]: {e}", self.path.display()))
            })? {
                Some(head) => {
                    let name = head.entry().filename.to_string_lossy().replace('\\', "/");
                    // extract_to/skip 消费 head：先取需要的字段
                    let encrypted = head.entry().is_encrypted();
                    let size = head.entry().unpacked_size;
                    if let Some(&ti) = remaining.get(&name) {
                        if encrypted && pw.is_none() {
                            return Err(VfsError::Password);
                        }
                        let dest = targets[ti].1;
                        // 缓存已存在且尺寸一致：跳过解压（幂等，密码候选重试时已解出的不重做）
                        if std::fs::metadata(dest)
                            .map(|m| m.len() == size)
                            .unwrap_or(false)
                        {
                            remaining.remove(&name);
                            file_done(ti);
                            if remaining.is_empty() {
                                return Ok(());
                            }
                            open = head.skip().map_err(|e| {
                                VfsError::Archive(format!("RAR 跳过条目 [{}] 失败: {e}", name))
                            })?;
                            continue;
                        }
                        // 解压到临时文件再改名：中断不留半截文件被当成有效缓存；
                        // 临时文件名带序号防并发踩踏。
                        // unrar 的 extract_to 无逐字节回调：写文件前发 0、完成后发
                        // 全量，UI 能区分"跳过阶段 → 写文件阶段"（字节数不串台）
                        progress(ExtractPhase::Target, 0, size);
                        let tmp = super::tmp_path(dest);
                        open = head.extract_to(&tmp).map_err(|e| {
                            std::fs::remove_file(&tmp).ok();
                            if is_password_err(&e) {
                                VfsError::Password
                            } else {
                                VfsError::Archive(format!(
                                    "RAR 解压失败: 压缩包 [{}] 条目 [{}] 解压到 [{}] 失败: {e}",
                                    self.path.display(),
                                    name,
                                    tmp.display()
                                ))
                            }
                        })?;
                        std::fs::rename(&tmp, dest).map_err(|e| {
                            std::fs::remove_file(&tmp).ok();
                            VfsError::Archive(format!(
                                "重命名临时文件失败: [{}] → [{}]: {e}",
                                tmp.display(),
                                dest.display()
                            ))
                        })?;
                        remaining.remove(&name);
                        progress(ExtractPhase::Target, size, size);
                        file_done(ti);
                        if remaining.is_empty() {
                            return Ok(());
                        }
                    } else {
                        open = head.skip().map_err(|e| {
                            VfsError::Archive(format!(
                                "RAR 跳过条目 [{}] 失败（正在定位目标）: {e}",
                                name
                            ))
                        })?;
                        skipped += 1;
                        skipped_bytes += size;
                        if skipped.is_multiple_of(64) {
                            // 静态批无 list 下标，跳过段无法算定比总量（total=0
                            // = 无分母，UI 显示活动指示）
                            progress(ExtractPhase::SkipUp, skipped_bytes, 0);
                        }
                    }
                }
                None => {
                    let names: Vec<String> = remaining.keys().cloned().collect();
                    return Err(VfsError::Archive(format!(
                        "entry 不存在: {}",
                        names.join(", ")
                    )));
                }
            }
        }
    }

    /// 会话单遍处理（单密码候选）：一次 processing 遍历动态消费 targets
    /// （try_batch 的会话版）。差异：目标集从 session 实时读取（UI 随时
    /// 增删），落地即 remove + 锁外回调；focus 漂移到已越过位置
    /// （upstream：focus 仍待解且其下标 < 当前遍历位置，即漂到上游）→
    /// Restart 提前返回，
    /// 外层从头重启遍历（unrar 无 folder 直达，重启是唯一回退手段）。
    /// 遍历尽不报"entry 不存在"——目标集动态，可能是 UI 刚加的目标在
    /// 已越过位置（外层下轮 Restart/重新遍历会处理），真不存在的由外层
    /// 无进展保险兜底
    fn session_pass(
        &self,
        pw: Option<&[u8]>,
        session: &SharedSession,
        name_idx: &std::collections::HashMap<String, usize>,
        list: &[VirtualEntry],
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
        file_done: &dyn Fn(usize, &Path),
    ) -> Result<(PassFlow, usize), VfsError> {
        let arch = match pw {
            Some(p) => Archive::with_password(&self.path, p),
            None => Archive::new(&self.path),
        };
        let mut open = arch.open_for_processing().map_err(|e| {
            VfsError::Archive(format!("打开 RAR 失败 [{}]: {e}", self.path.display()))
        })?;
        // 分段总量（pass 开工前一次算好）：RAR 顺序解码必然消费 [0, 最后目标]
        // 的整段流——以 focus（抢道锚点）为界，目标 → Upstream/Target/Downstream，
        // 非目标 → SkipUp/Downstream（段内夹的过滤条目/缓存命中条目计入总量，
        // 才能让 bar 一直有分母可走）。开工即发各段 (0, total)，条立即出现不空窗
        // mut：驻留唤醒顺流续解时按新批 focus 重置 + 分母重算（唤醒臂）
        let (mut focus_idx, mut phase_totals) = {
            let st = session.lock().unwrap();
            let focus = st
                .focus
                .unwrap_or_else(|| st.targets.values().map(|(i, _)| *i).min().unwrap_or(0));
            let tgt_idx: std::collections::HashSet<usize> =
                st.targets.values().map(|(i, _)| *i).collect();
            let last = tgt_idx.iter().copied().max().unwrap_or(0);
            let mut t = [(0u64, 0u64); ExtractPhase::COUNT];
            for (i, e) in list.iter().enumerate().take(last + 1) {
                if tgt_idx.contains(&i) {
                    match i.cmp(&focus) {
                        std::cmp::Ordering::Less => t[ExtractPhase::Upstream as usize].1 += e.size,
                        std::cmp::Ordering::Equal => t[ExtractPhase::Target as usize] = (0, e.size),
                        std::cmp::Ordering::Greater => {
                            t[ExtractPhase::Downstream as usize].1 += e.size
                        }
                    }
                } else {
                    match i.cmp(&focus) {
                        std::cmp::Ordering::Less => t[ExtractPhase::SkipUp as usize].1 += e.size,
                        std::cmp::Ordering::Greater => {
                            t[ExtractPhase::Downstream as usize].1 += e.size
                        }
                        std::cmp::Ordering::Equal => {}
                    }
                }
            }
            (focus, t)
        };
        progress(ExtractPhase::Locate, 0, 0);
        for p in [
            ExtractPhase::SkipUp,
            ExtractPhase::Upstream,
            ExtractPhase::Target,
            ExtractPhase::Downstream,
        ] {
            let (c, t) = phase_totals[p as usize];
            if t > 0 {
                progress(p, c, t);
            }
        }
        let mut landed = 0; // 本遍落地数
        let mut skipped: usize = 0;
        let mut skip_cur: u64 = 0;
        let mut up_cur: u64 = 0;
        let mut down_cur: u64 = 0;
        loop {
            // 逐条目定位期间检查取消（大包 / NAS 上定位耗时分钟级）
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(VfsError::Cancelled);
            }
            let head = match open.read_header().map_err(|e| {
                VfsError::Archive(format!("读 RAR 头失败 [{}]: {e}", self.path.display()))
            })? {
                Some(h) => h,
                None => return Ok((PassFlow::Done, landed)),
            };
            let name = head.entry().filename.to_string_lossy().replace('\\', "/");
            // extract_to/skip 消费 head：先取需要的字段
            let encrypted = head.entry().is_encrypted();
            let size = head.entry().unpacked_size;
            let cur_idx = name_idx.get(&name).copied();
            // ---- 条目边界驻留（真暂停）：read_header 已返回 Some、head 未
            // 消费——控制权在 Rust 侧，阻塞睡眠只是持有 unrar Open 的普通
            // 线程睡眠（Phase A 实证：Open 闲置 35s 后 skip/extract 功能
            // 完好、闲置期 CPU 增量 0ms）。targets 空 = 整会话解净：睡眠等
            // 新目标而非退出线程，唤醒后从当前位置零前缀续跑（unrar 无定位
            // 能力，解码状态只活在这份 Open 里——驻留必须发生在本函数）。
            // 睡眠分片检查 cancel，不持锁睡眠；醒来重读纪律：tgt/upstream
            // 在下方重新持锁快照，禁止沿用睡眠前决策
            let parked = session.lock().unwrap().targets.is_empty();
            if parked {
                // 驻留等待无消费动作，跳过提示过期（对齐 7z 驻留清空）
                session.lock().unwrap().skip_note = None;
                tracing::debug!(
                    "[会话] 条目[{name}]（idx {cur_idx:?}）目标清空 → RAR 条目边界驻留等待"
                );
                // 驻留睡眠 + 唤醒分流循环：read_header(self) 已消费 open、
                // 此刻 head 持有解码状态——醒来目标又被换空的兜底只能原地
                // 重睡，无法回到条目循环头（那会 drop head 丢掉 open）
                let cur = cur_idx.unwrap_or(usize::MAX);
                let min_tgt = loop {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err(VfsError::Cancelled);
                    }
                    let still_empty = session.lock().unwrap().targets.is_empty();
                    if still_empty {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        continue;
                    }
                    let m = {
                        let st = session.lock().unwrap();
                        st.targets.values().map(|(i, _)| *i).min()
                    };
                    // 醒来瞬间目标又被换空（UI 重定向 clear+写同锁原子，
                    // 理论不可见；防御性兜底）：原地重睡
                    let Some(m) = m else {
                        continue;
                    };
                    break m;
                };
                if min_tgt < cur {
                    // 唤醒分流：min(targets) < 当前条目 = 新批含遍历位置上游
                    // 目标 → Restart 回跳重启（unrar 无定位能力，前缀重解不
                    // 可避免）；min ≥ cur = 全在下游（含唤醒点条目本身）→
                    // 继续当前遍历（零前缀重解，驻留核心收益）。驻留唤醒
                    // Restart 与漂移抢道 Restart 共用外层同一 restarts 计数器
                    //（合并计数），防 UI 连续上游点击打满 64 上限误报
                    tracing::debug!(
                        "[会话] RAR 驻留唤醒（上游）：新批含遍历位置身前目标 → Restart 回跳重启"
                    );
                    return Ok((PassFlow::Restart, landed));
                }
                // 唤醒臂：新鲜 focus/targets 重算段骨架（分母从当前条目起，
                // 身前条目已流过解码流不进分母）+ resume 置位（UI 标题切
                // "续解中"）+ 段游标清零（分母口径已换）。求和无锁依赖先放锁，
                // 防 2021 edition 守卫临时值跨块重入自死锁（教训见 7z 改判分支）
                let (fresh_focus, wake_tgt) = {
                    let mut st = session.lock().unwrap();
                    st.resume = true;
                    let fresh_focus = st.focus.unwrap_or_else(|| {
                        st.targets.values().map(|(i, _)| *i).min().unwrap_or(0)
                    });
                    let wake_tgt: std::collections::HashSet<usize> =
                        st.targets.values().map(|(i, _)| *i).collect();
                    (fresh_focus, wake_tgt)
                };
                focus_idx = fresh_focus;
                let last = wake_tgt.iter().copied().max().unwrap_or(0);
                let mut t = [(0u64, 0u64); ExtractPhase::COUNT];
                for (i, e) in list.iter().enumerate().skip(cur).take(last + 1 - cur) {
                    if wake_tgt.contains(&i) {
                        match i.cmp(&fresh_focus) {
                            std::cmp::Ordering::Less => {
                                t[ExtractPhase::Upstream as usize].1 += e.size
                            }
                            std::cmp::Ordering::Equal => {
                                t[ExtractPhase::Target as usize] = (0, e.size)
                            }
                            std::cmp::Ordering::Greater => {
                                t[ExtractPhase::Downstream as usize].1 += e.size
                            }
                        }
                    } else {
                        match i.cmp(&fresh_focus) {
                            std::cmp::Ordering::Less => {
                                t[ExtractPhase::SkipUp as usize].1 += e.size
                            }
                            std::cmp::Ordering::Greater => {
                                t[ExtractPhase::Downstream as usize].1 += e.size
                            }
                            std::cmp::Ordering::Equal => {}
                        }
                    }
                }
                phase_totals = t;
                skipped = 0;
                skip_cur = 0;
                up_cur = 0;
                down_cur = 0;
                // 开工协议同款：Locate(0,0) + 各非零段 (0,total)
                progress(ExtractPhase::Locate, 0, 0);
                for p in [
                    ExtractPhase::SkipUp,
                    ExtractPhase::Upstream,
                    ExtractPhase::Target,
                    ExtractPhase::Downstream,
                ] {
                    let (_, tot) = phase_totals[p as usize];
                    if tot > 0 {
                        progress(p, 0, tot);
                    }
                }
            }
            // 持锁快照：本条目目标 / focus 是否漂移到当前遍历位置的上游
            let (tgt, upstream) = {
                let st = session.lock().unwrap();
                let tgt = st.targets.get(&name).cloned();
                // focus 仍待解（在下标相等的目标里）且在当前遍历位置之前
                let upstream = st.focus.is_some_and(|f| {
                    st.targets.values().any(|(idx, _)| *idx == f) && cur_idx.is_some_and(|c| f < c)
                });
                (tgt, upstream)
            };
            if upstream {
                return Ok((PassFlow::Restart, landed)); // 回退重启定位 focus
            }
            let Some((ti, dest)) = tgt else {
                // 非目标条目：focus 之前 = 跳过段（解码丢弃），之后 = 下游段
                // （整段流进度的一部分，计入推进防止夹缝处 bar 停滞）。
                // 跳过提示：消费边界短锁刷新（solid RAR 逐条 skip 是真解码，
                // 长视频跳过期让 UI 转圈可见在干什么）
                session.lock().unwrap().skip_note =
                    Some((cur_idx.unwrap_or(usize::MAX), name.clone(), size));
                let is_up = cur_idx.is_some_and(|c| c < focus_idx);
                open = head.skip().map_err(|e| {
                    VfsError::Archive(format!("RAR 跳过条目 [{}] 失败（正在定位目标）: {e}", name))
                })?;
                skipped += 1;
                if is_up {
                    skip_cur += size;
                    if skipped.is_multiple_of(64) {
                        let (_, t) = phase_totals[ExtractPhase::SkipUp as usize];
                        progress(ExtractPhase::SkipUp, skip_cur.min(t), t);
                    }
                } else {
                    down_cur += size;
                    if skipped.is_multiple_of(64) {
                        let (_, t) = phase_totals[ExtractPhase::Downstream as usize];
                        progress(ExtractPhase::Downstream, down_cur.min(t), t);
                    }
                }
                continue;
            };
            if encrypted && pw.is_none() {
                return Err(VfsError::Password);
            }
            // 缓存已存在且尺寸一致：幂等跳过（候选重试/会话重启后已解出的不重做）。
            // 该条目本应解压：全量计入其所属段的推进（否则缓存命中的夹缝让 bar 停滞）
            if std::fs::metadata(&dest)
                .map(|m| m.len() == size)
                .unwrap_or(false)
            {
                match ti.cmp(&focus_idx) {
                    std::cmp::Ordering::Less => {
                        up_cur += size;
                        let (_, t) = phase_totals[ExtractPhase::Upstream as usize];
                        progress(ExtractPhase::Upstream, up_cur.min(t), t);
                    }
                    std::cmp::Ordering::Greater => {
                        down_cur += size;
                        let (_, t) = phase_totals[ExtractPhase::Downstream as usize];
                        progress(ExtractPhase::Downstream, down_cur.min(t), t);
                    }
                    std::cmp::Ordering::Equal => {
                        let t = size;
                        progress(ExtractPhase::Target, t, t);
                    }
                }
                let hit = {
                    let mut st = session.lock().unwrap();
                    st.skip_note = None; // 幂等落地，跳过提示过期
                    st.targets.remove(&name).is_some()
                };
                if hit {
                    file_done(ti, &dest); // 锁已释放
                    landed += 1;
                }
                open = head
                    .skip()
                    .map_err(|e| VfsError::Archive(format!("RAR 跳过条目 [{}] 失败: {e}", name)))?;
                continue;
            }
            // 分段归属：目标下标 vs focus（unrar 无逐字节回调，各段只能
            // 写前发基线、完成后发全量）
            let phase = match ti.cmp(&focus_idx) {
                std::cmp::Ordering::Equal => ExtractPhase::Target,
                std::cmp::Ordering::Less => ExtractPhase::Upstream,
                std::cmp::Ordering::Greater => ExtractPhase::Downstream,
            };
            let (base, seg_total) = match phase {
                ExtractPhase::Target => (0, size),
                ExtractPhase::Upstream => (
                    up_cur,
                    phase_totals[ExtractPhase::Upstream as usize].1,
                ),
                ExtractPhase::Downstream => (
                    down_cur,
                    phase_totals[ExtractPhase::Downstream as usize].1,
                ),
                _ => (0, 0),
            };
            progress(phase, base.min(seg_total), seg_total);
            let tmp = super::tmp_path(&dest);
            open = head.extract_to(&tmp).map_err(|e| {
                std::fs::remove_file(&tmp).ok();
                if is_password_err(&e) {
                    VfsError::Password
                } else {
                    VfsError::Archive(format!(
                        "RAR 解压失败: 压缩包 [{}] 条目 [{}] 解压到 [{}] 失败: {e}",
                        self.path.display(),
                        name,
                        tmp.display()
                    ))
                }
            })?;
            std::fs::rename(&tmp, &dest).map_err(|e| {
                std::fs::remove_file(&tmp).ok();
                VfsError::Archive(format!(
                    "重命名临时文件失败: [{}] → [{}]: {e}",
                    tmp.display(),
                    dest.display()
                ))
            })?;
            let hit = {
                let mut st = session.lock().unwrap();
                st.skip_note = None; // 目标落地，跳过提示过期
                st.targets.remove(&name).is_some()
            };
            if hit {
                file_done(ti, &dest); // 锁已释放
                landed += 1;
            }
            // 完成后该段推进全量（unrar 无逐字节回调，条目级粒度）
            match phase {
                ExtractPhase::Target => progress(ExtractPhase::Target, size, size),
                ExtractPhase::Upstream => {
                    up_cur += size;
                    let (_, t) = phase_totals[ExtractPhase::Upstream as usize];
                    progress(ExtractPhase::Upstream, up_cur.min(t), t);
                }
                ExtractPhase::Downstream => {
                    down_cur += size;
                    let (_, t) = phase_totals[ExtractPhase::Downstream as usize];
                    progress(ExtractPhase::Downstream, down_cur.min(t), t);
                }
                _ => {}
            }
            // targets 解净不再提前收工：下一边界驻留等待新目标（条目边界
            // 驻留分支接管）；若已是包尾则 read_header 返回 None 走 Done，
            // 外层按"包尾不驻留"退出线程
        }
    }
}

impl ArchiveVFS for RarVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        let Ok(mut open) = self.archive().open_for_listing() else {
            return vec![];
        };
        let mut entries = Vec::new();
        loop {
            match open.read_header() {
                Ok(Some(head)) => {
                    let h = head.entry();
                    // 分卷条目在 List 模式下每卷出现一次，SPLIT_AFTER 的卷条目跳过（只记最终条目）
                    if !h.is_split_after() {
                        entries.push(VirtualEntry {
                            // 统一分隔符为 '/'
                            name: h.filename.to_string_lossy().replace('\\', "/"),
                            size: h.unpacked_size,
                            is_dir: h.is_directory(),
                            encrypted: h.is_encrypted(),
                            // RAR 头自带未压缩数据 CRC（VarTidy 包内去重判据）
                            crc: Some(h.file_crc),
                        });
                    }
                    match head.skip() {
                        Ok(o) => open = o,
                        Err(_) => break,
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        entries
    }

    fn open_entry(&self, _entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        Err(VfsError::Unsupported(
            "RAR 不支持流式读取（solid 走解压会话，非 solid 走单文件按需解压）".into(),
        ))
    }

    fn access_mode(&self) -> AccessMode {
        // solid RAR：文件数据首尾相接成一条顺序解码流 → 解压会话 + 滑窗预取；
        // 非 solid RAR（rar 默认 -s-）：每文件独立压缩，unrar 定位 = 廉价头
        // 跳转，随机访问只需解点击项本身 → 单文件按需（历史版本行为），
        // 不付滑窗/预取/重定向的成本
        if self.solid {
            AccessMode::Solid
        } else {
            AccessMode::SingleFile
        }
    }

    /// 上游预取免费区 = 整个前缀 [0, 目标)：**仅 solid RAR 有意义**——unrar
    /// 定位目标需逐条 skip，solid 前缀是真解码，把顺带解出的条目写盘零增量。
    /// 非 solid RAR 返回 None：skip 是廉价头跳转，上游预取等于白白解码
    /// 前置文件。上限由 config `extract.prefetch_upstream_max_mb` 封顶（伴随量
    /// 非配额，实取以整个前缀大小为准）
    fn locate_start(&self, _entry_idx: usize) -> Option<usize> {
        if self.solid {
            Some(0)
        } else {
            None
        }
    }

    fn extract_entry(
        &self,
        entry: &VirtualEntry,
        dest: &Path,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
    ) -> Result<(), VfsError> {
        self.extract_batch(&[(entry, dest)], cancel, progress, &|_| {})
    }

    fn extract_batch(
        &self,
        targets: &[(&VirtualEntry, &Path)],
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
        file_done: &dyn Fn(usize),
    ) -> Result<(), VfsError> {
        // 尝试序列：有密码 → 各编码候选，最后兜底无密码（条目实际未加密时直接成功）；
        // 无密码 → 仅无密码（加密条目在 try_batch 内报 Password，UI 弹密码框）
        let mut attempts: Vec<Option<Vec<u8>>> = Vec::new();
        if self.password.is_empty() {
            attempts.push(None);
        } else {
            for c in &self.password {
                attempts.push(Some(c.clone()));
            }
            attempts.push(None);
        }
        let mut last_err = VfsError::Password;
        for pw in &attempts {
            // 多候选/大包定位期间检查取消（unrar 逐条目 skip 定位，大包耗时）
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(VfsError::Cancelled);
            }
            match self.try_batch(pw.as_deref(), targets, cancel, progress, file_done) {
                Ok(()) => return Ok(()),
                // 密码/编码不匹配（RAR4 无密码哈希，错密码可能只报 CRC 失败）：换下一候选。
                // 已解出的目标已落地（try_batch 幂等跳过），重试只补漏
                Err(e) if is_pwd_or_crc(&e) => last_err = e,
                Err(e) => return Err(e),
            }
        }
        Err(last_err)
    }

    /// 持久化解码会话（trait 协议见 vfs/mod.rs）：session_pass 外再套两层——
    /// 候选循环（密码/编码不匹配换候选，同 extract_batch）内含 Restart 重试
    /// （focus 回跳重启定位，上限 64 次防 UI 抖动死循环），外层规划轮在一轮
    /// 遍历尽后重读 targets（UI 新加的目标进下一轮）。空目标不退出线程：
    /// pass 内条目边界驻留等待新目标（解码器不销毁，唤醒零前缀续跑）；唯一
    /// 例外是包尾（遍历尽且 targets 空）——解码流已耗尽，驻留无收益，线程
    /// 退出下次点击重新 spawn。cancel 是硬退出。连续两轮零落地且 targets 仍
    /// 非空 = 真的 entry 不存在（单轮零落地可能是瞬态：遍历尽与 UI 加目标
    /// 竞争），报错防死循环
    fn extract_session(
        &self,
        session: &SharedSession,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
        file_done: &dyn Fn(usize, &Path),
    ) -> Result<(), VfsError> {
        // 尝试序列：有密码 → 各编码候选 + 无密码兜底；无密码 → 仅无密码（同 extract_batch）
        let mut attempts: Vec<Option<Vec<u8>>> = Vec::new();
        if self.password.is_empty() {
            attempts.push(None);
        } else {
            for c in &self.password {
                attempts.push(Some(c.clone()));
            }
            attempts.push(None);
        }
        // list 下标与条目尺寸表：name_idx 供 upstream 判定，list 供分段总量
        // 统计（包内容不变，主循环外构建一次）
        let list = self.list();
        let name_idx: std::collections::HashMap<String, usize> = list
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.clone(), i))
            .collect();
        let mut stagnant = 0; // 连续零落地轮数（无进展保险）
        loop {
            // 持锁预检：取消 / 规划代数快照（判空与置 alive=false 同锁，协议见
            // mod.rs）。顺带快照本轮 gen——UI 重定向同锁自增，零落地轮末
            // 比对可知"没进展"是否因中途换了目标
            let round_gen = {
                let mut st = session.lock().unwrap();
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    st.alive = false;
                    return Err(VfsError::Cancelled);
                }
                // targets 判空退出已由 pass 内条目边界驻留取代（预检空目标
                // 照常进入 pass：首个条目边界驻留等新目标，cancel 唯一硬退出）。
                // 规划（重）开工 = 从头遍历重解前缀（非续解）：续解标志复位
                // （驻留唤醒臂再置 true，UI 据此切换标题文案）
                st.resume = false;
                st.gen
            };
            let mut landed = 0;
            // 候选轮内重启次数：漂移抢道 Restart 与驻留唤醒上游回跳 Restart
            // 合并计数（上限 64 防 UI 抖动/连点上游死循环误报）
            let mut restarts = 0;
            let mut all_done = false;
            let mut last_err = VfsError::Password;
            'cand: for pw in &attempts {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    super::session_exit(session);
                    return Err(VfsError::Cancelled);
                }
                loop {
                    match self.session_pass(
                        pw.as_deref(),
                        session,
                        &name_idx,
                        &list,
                        cancel,
                        progress,
                        file_done,
                    ) {
                        Ok((PassFlow::Done, n)) => {
                            landed += n;
                            all_done = true;
                            break 'cand;
                        }
                        Ok((PassFlow::Restart, n)) => {
                            landed += n;
                            restarts += 1;
                            // 重启 = 从头遍历重解前缀（非续解）：续解标志复位
                            //（唤醒臂再置 true，UI 据此切换标题文案）
                            session.lock().unwrap().resume = false;
                            if restarts > 64 {
                                super::session_exit(session);
                                return Err(VfsError::Archive(
                                    "RAR 会话定位重启次数过多（focus 频繁回跳）".into(),
                                ));
                            }
                        }
                        // 密码/编码不匹配：跳出 Restart 重试换下一候选
                        // （已落地的幂等跳过，重试只补漏）
                        Err(e) if is_pwd_or_crc(&e) => {
                            last_err = e;
                            break;
                        }
                        Err(e) => {
                            super::session_exit(session);
                            return Err(e);
                        }
                    }
                }
            }
            if !all_done {
                super::session_exit(session);
                return Err(last_err);
            }
            // 包尾不驻留（遍历尽且 targets 空 = 整包解码流已耗尽，驻留无续解
            // 收益）：线程退出，下次点击重新 spawn（与驻留改造前的空闲语义
            // 一致）。targets 非空 = 有目标没解出（可能 UI 刚加在已越过位置），
            // 进入无进展保险与重规划
            {
                let mut st = session.lock().unwrap();
                if st.targets.is_empty() {
                    st.alive = false;
                    return Ok(());
                }
            }
            // 无进展保险：连续两轮零落地且 targets 仍非空 → 真的不在包内；
            // 有落地重置计数，目标已空则外层预检正常收尾。本轮中途 UI 重
            // 定向过（gen 已变）的零落地不计入——目标已换，旧计数作废
            if landed == 0 {
                let (cur_gen, targets_left) = {
                    let st = session.lock().unwrap();
                    (st.gen, !st.targets.is_empty())
                };
                if cur_gen != round_gen {
                    stagnant = 0;
                } else {
                    stagnant += 1;
                    if stagnant >= 2 && targets_left {
                        super::session_exit(session);
                        return Err(VfsError::Archive("RAR 会话无进展：目标条目不在包内".into()));
                    }
                }
            } else {
                stagnant = 0;
            }
            // 有落地或瞬态零落地：外层重规划（UI 新加的目标进下一轮）
        }
    }

    /// 密码校验：对编码候选（UTF-8 / 配置代码页，见 vfs::password_candidates）
    /// 逐个试 verify_one——unrar 的密码是原始字节，编码不对与密码不对同样报错。
    /// 密码/编码不匹配换下一候选，全部失败才报 Password
    fn verify_password(
        &self,
        entry: &VirtualEntry,
        password: Option<&str>,
    ) -> Result<(), VfsError> {
        let candidates: Vec<Option<Vec<u8>>> = match password {
            None => vec![None],
            Some(pw) => super::password_candidates(pw)
                .into_iter()
                .map(Some)
                .collect(),
        };
        let mut last = VfsError::Password;
        for c in &candidates {
            match self.verify_one(entry, c.as_deref()) {
                Ok(()) => return Ok(()),
                Err(e) if is_pwd_or_crc(&e) => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

/// RAR4 不存密码哈希：错密码解出乱码常表现为 CRC 失败而非"password"——
/// 密码错误与 CRC 失败都值得换下一个编码候选再试
fn is_pwd_or_crc(e: &VfsError) -> bool {
    if matches!(e, VfsError::Password) {
        return true;
    }
    let s = e.to_string().to_lowercase();
    s.contains("password") || s.contains("crc")
}

/// 会话单遍处理的结果：Done = 本候选遍历尽（能做的都做完）；
/// Restart = focus 漂移到已越过位置，需从头重启遍历回退定位
enum PassFlow {
    Done,
    Restart,
}

impl RarVFS {
    /// 单个密码候选（字节形态）的校验：open_for_processing 逐头定位目标条目，
    /// extract_to 临时文件（unrar 无内存 sink；探测目标是最小加密条目，代价可控）。
    /// RAR5 头带 PswCheck、错密码即报；RAR4 无密码哈希、靠解压结果 CRC 判错
    fn verify_one(&self, entry: &VirtualEntry, pw: Option<&[u8]>) -> Result<(), VfsError> {
        let arch = match pw {
            Some(p) => Archive::with_password(&self.path, p),
            None => Archive::new(&self.path),
        };
        let tmp = super::tmp_path(
            &std::env::temp_dir()
                .join(format!("archview_pw_probe_{}.tmp", std::process::id())),
        );
        let mut open = arch.open_for_processing().map_err(|e| {
            VfsError::Archive(format!("打开 RAR 失败 [{}]: {e}", self.path.display()))
        })?;
        loop {
            match open.read_header().map_err(|e| {
                VfsError::Archive(format!("读 RAR 头失败 [{}]: {e}", self.path.display()))
            })? {
                Some(head) => {
                    let name = head.entry().filename.to_string_lossy().replace('\\', "/");
                    if name == entry.name {
                        let r = head.extract_to(&tmp).map_err(|e| {
                            let mapped = if is_password_err(&e) {
                                VfsError::Password
                            } else {
                                VfsError::Archive(format!(
                                    "RAR 校验解压失败: 压缩包 [{}] 条目 [{}]: {e}",
                                    self.path.display(),
                                    name
                                ))
                            };
                            // RAR4 错密码常表现为 CRC 失败（无密码哈希），同样按密码错处理
                            if is_pwd_or_crc(&mapped) {
                                VfsError::Password
                            } else {
                                mapped
                            }
                        });
                        std::fs::remove_file(&tmp).ok();
                        return r.map(|_| ());
                    }
                    open = head.skip().map_err(|e| {
                        VfsError::Archive(format!("RAR 跳过条目 [{}] 失败: {e}", name))
                    })?;
                }
                None => return Err(VfsError::Archive(format!("entry 不存在: {}", entry.name))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAR 夹具由 cargo run --bin smoke_setup 预生成于 smoke/rar/
    /// （WinRAR rar.exe 造包，payload 与断言逐字节一致），缺失即 panic
    /// 禁止静默跳过（src/smoke.rs）。解压产物落本测试独立 scratch 目录，
    /// 不写夹具目录。注：解压运行时用 unrar crate（内置编译），与 rar.exe 无关。

    /// 每测试独立 scratch 目录（pid 隔离，防并行抢占）
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rar_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn list_and_extract() {
        let arch = crate::smoke::require("rar/test.rar");
        let dir = scratch("single");

        let vfs = RarVFS::open(&arch, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.bin");
        assert_eq!(entries[0].size, 50_000);

        let dest = dir.join("out.bin");
        vfs.extract_entry(
            &entries[0],
            &dest,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len(), 50_000);
        assert!(data.iter().all(|&b| b == 9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非固态 RAR（rar 默认）→ AccessMode::SingleFile：随机访问只解点击项，
    /// 无上游免费区（skip 是廉价头跳转，整前缀预取等于白白解码）。中间条目
    /// 直接解压成功 = 真随机访问语义（不等价于顺序流定位）
    #[test]
    fn non_solid_rar_routes_single_file() {
        let arch = crate::smoke::require("rar/plain.rar");
        let dir = scratch("non_solid");
        let vfs = RarVFS::open(&arch, None).unwrap();
        assert_eq!(
            vfs.access_mode(),
            AccessMode::SingleFile,
            "rar 默认非固态，应走单文件按需"
        );
        assert_eq!(
            vfs.locate_start(0),
            None,
            "非固态无上游免费区：不应返回整前缀 Some(0)"
        );

        let entries = vfs.list();
        assert_eq!(entries.len(), 3);
        let dest = dir.join("mid.bin");
        vfs.extract_entry(
            &entries[1], // 中间的 f1.bin：非固态随机访问应直达
            &dest,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len(), 30_000);
        assert!(data.iter().all(|&b| b == 8), "解出应为本条目内容（填充 8）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 固态 RAR（rar a -s）→ AccessMode::Solid：解压会话滑窗路径 + 整前缀
    /// 上游免费区（locate_start = Some(0)）；单遍 processing 跨中间条目解
    /// 头尾两目标（skip 非目标、顺带解目标）
    #[test]
    fn solid_rar_routes_solid_and_batch_extracts() {
        let arch = crate::smoke::require("rar/solid.rar");
        let dir = scratch("solid");
        let vfs = RarVFS::open(&arch, None).unwrap();
        assert_eq!(vfs.access_mode(), AccessMode::Solid, "-s 打包应判为固态");
        assert_eq!(
            vfs.locate_start(2),
            Some(0),
            "固态包上游免费区 = 整个前缀 [0, 目标)"
        );

        let entries = vfs.list();
        assert_eq!(entries.len(), 3);
        let d0 = dir.join("o0.bin");
        let d2 = dir.join("o2.bin");
        vfs.extract_batch(
            &[(&entries[0], &d0), (&entries[2], &d2)],
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
            &|_| {},
        )
        .unwrap();
        for (p, fill) in [(&d0, 7u8), (&d2, 9u8)] {
            let data = std::fs::read(p).unwrap();
            assert_eq!(data.len(), 30_000);
            assert!(data.iter().all(|&b| b == fill), "解出应为本条目内容");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- extract_session 驻留语义测试（镜像 7z 组，协议见 sevenz_vfs.rs）----

    /// 构造空闲会话（alive=false；UI 组批时写 targets + focus + 置 true，
    /// 再调用 extract_session —— 协议见 vfs/mod.rs SessionState）
    fn new_session() -> SharedSession {
        std::sync::Arc::new(std::sync::Mutex::new(crate::vfs::SessionState {
            targets: Default::default(),
            focus: None,
            alive: false,
            gen: 0,
            skip_note: None,
            resume: false,
        }))
    }

    /// 条目边界驻留：批解净后线程不退出（unrar Open 不销毁，下次下游点击
    /// 零前缀续跑），驻留等待至 cancel 收尾；驻留期清空 skip_note、resume
    /// 保持 false（未唤醒续解）
    #[test]
    fn rar_session_parks_at_entry_boundary() {
        let arch = crate::smoke::require("rar/solid.rar");
        let dir = scratch("sess_park");
        let vfs = RarVFS::open(&arch, None).unwrap();
        let s = new_session();
        let d0 = dir.join("out_f0.bin");
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f0.bin".into(), (0, d0.clone()));
            st.focus = Some(0);
            st.alive = true;
        }
        let landed: std::sync::Arc<std::sync::Mutex<Vec<usize>>> = Default::default();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, cancel2, landed2) = (s.clone(), cancel.clone(), landed.clone());
        let handle = std::thread::spawn(move || {
            vfs.extract_session(&s2, &cancel2, &|_, _, _| {}, &|idx, _| {
                landed2.lock().unwrap().push(idx);
            })
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !landed.lock().unwrap().contains(&0) {
            assert!(std::time::Instant::now() < deadline, "f0 落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // 留出余量确保线程推进到 f1 边界驻留（睡眠分片 50ms）
        std::thread::sleep(std::time::Duration::from_millis(300));
        {
            let st = s.lock().unwrap();
            assert!(st.targets.is_empty(), "解净后 targets 应清空");
            assert!(st.alive, "条目边界驻留后线程不应退出");
            assert!(!st.resume, "驻留等待期 resume 应为 false（未唤醒续解）");
            assert!(st.skip_note.is_none(), "驻留应清空跳过提示");
        }
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = handle.join().unwrap();
        assert!(
            matches!(r, Err(VfsError::Cancelled)),
            "驻留会话应由 cancel 收尾: {r:?}"
        );
        let data = std::fs::read(&d0).unwrap();
        assert_eq!(data.len(), 30_000);
        assert!(data.iter().all(|&b| b == 7), "f0 内容错误");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 驻留唤醒（下游）：驻留后 UI 重定向新批（唤醒点条目 f1 + 下游 f2）
    /// → 零前缀续跑：唤醒点条目改判目标当场落地 + 下游顺流落地；唤醒臂发
    /// 新段骨架（Downstream total==30000，防"清零转圈无分母"回归）；resume
    /// 置 true（UI 标题切"续解中"——Restart 重启路径会复位它，此处为真即
    /// 证明未误走重启）。f2 是包尾条目：解净后遍历尽 → 包尾不驻留，线程
    /// 正常退出（alive=false）
    #[test]
    fn rar_session_wake_downstream_resumes() {
        let arch = crate::smoke::require("rar/solid.rar");
        let dir = scratch("sess_wake_down");
        let vfs = RarVFS::open(&arch, None).unwrap();
        let s = new_session();
        let d0 = dir.join("out_f0.bin");
        let d1 = dir.join("out_f1.bin");
        let d2 = dir.join("out_f2.bin");
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f0.bin".into(), (0, d0.clone()));
            st.focus = Some(0);
            st.alive = true;
        }
        let landed: std::sync::Arc<std::sync::Mutex<Vec<usize>>> = Default::default();
        let reports: std::sync::Arc<
            std::sync::Mutex<Vec<(ExtractPhase, u64, u64)>>,
        > = Default::default();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, cancel2, landed2, rep) =
            (s.clone(), cancel.clone(), landed.clone(), reports.clone());
        let handle = std::thread::spawn(move || {
            vfs.extract_session(
                &s2,
                &cancel2,
                &move |ph, cur, total| {
                    rep.lock().unwrap().push((ph, cur, total));
                },
                &move |idx, _| {
                    landed2.lock().unwrap().push(idx);
                },
            )
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !landed.lock().unwrap().contains(&0) {
            assert!(std::time::Instant::now() < deadline, "f0 落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            s.lock().unwrap().alive && s.lock().unwrap().targets.is_empty(),
            "f0 落地后应进入 f1 边界驻留"
        );
        // 模拟 UI 重定向：新批 f1（唤醒点条目，改判落地）+ f2（下游），focus 推进
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f1.bin".into(), (1, d1.clone()));
            st.targets.insert("f2.bin".into(), (2, d2.clone()));
            st.focus = Some(1);
        }
        // 判据1：唤醒臂骨架 Downstream (0, 30000) 出现（旧实现清零全程无分母）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let hit = reports
                .lock()
                .unwrap()
                .iter()
                .any(|&(ph, _, t)| ph == ExtractPhase::Downstream && t == 30_000);
            if hit {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "唤醒后未见 Downstream 非零骨架：唤醒臂段总量重算回归"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 判据2：唤醒续解标志（Restart 重启路径会复位 resume，此处为真证明零前缀续跑）
        assert!(s.lock().unwrap().resume, "唤醒续解后 resume 应为 true");
        // 判据3：唤醒点条目改判落地 + 下游顺流落地，顺序 [0,1,2]
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !landed.lock().unwrap().contains(&1) || !landed.lock().unwrap().contains(&2) {
            assert!(std::time::Instant::now() < deadline, "f1/f2 顺流落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(*landed.lock().unwrap(), vec![0, 1, 2], "落地顺序错误");
        let r = handle.join().unwrap();
        assert!(r.is_ok(), "包尾解净应正常退出线程: {r:?}");
        assert!(!s.lock().unwrap().alive, "包尾解净 alive 应为 false");
        for (p, fill) in [(&d1, 8u8), (&d2, 9u8)] {
            let data = std::fs::read(p).unwrap();
            assert_eq!(data.len(), 30_000);
            assert!(data.iter().all(|&b| b == fill), "唤醒续解内容错误");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 驻留唤醒（上游）：驻留后 UI 加遍历位置上游目标 → Restart 回跳重启
    /// （unrar 无定位能力，前缀重解不可避免）；重启后上游目标落地、resume
    /// 不置位（重启 = 前缀重解非续解）；重启轮 f1 幂等跳过不重复落地
    #[test]
    fn rar_session_wake_upstream_restarts() {
        let arch = crate::smoke::require("rar/solid.rar");
        let dir = scratch("sess_wake_up");
        let vfs = RarVFS::open(&arch, None).unwrap();
        let s = new_session();
        let d0 = dir.join("out_f0.bin");
        let d1 = dir.join("out_f1.bin");
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f1.bin".into(), (1, d1.clone()));
            st.focus = Some(1);
            st.alive = true;
        }
        let landed: std::sync::Arc<std::sync::Mutex<Vec<usize>>> = Default::default();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, cancel2, landed2) = (s.clone(), cancel.clone(), landed.clone());
        let handle = std::thread::spawn(move || {
            vfs.extract_session(&s2, &cancel2, &|_, _, _| {}, &|idx, _| {
                landed2.lock().unwrap().push(idx);
            })
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !landed.lock().unwrap().contains(&1) {
            assert!(std::time::Instant::now() < deadline, "f1 落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            s.lock().unwrap().alive && s.lock().unwrap().targets.is_empty(),
            "f1 落地后应进入 f2 边界驻留"
        );
        // 模拟 UI 回看：新目标 f0 在遍历位置（f2 边界）上游
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f0.bin".into(), (0, d0.clone()));
            st.focus = Some(0);
        }
        // 判据：f0 落地——驻留点已越过 f0，只能经 Restart 回跳重解前缀
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !landed.lock().unwrap().contains(&0) {
            assert!(std::time::Instant::now() < deadline, "Restart 后 f0 落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(*landed.lock().unwrap(), vec![1, 0], "落地顺序错误");
        assert!(
            !s.lock().unwrap().resume,
            "重启路径 = 前缀重解，resume 应保持 false"
        );
        // f0 落地后 targets 空，重启轮继续遍历至 f2 边界 → 再次驻留，cancel 收尾
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = handle.join().unwrap();
        assert!(
            matches!(r, Err(VfsError::Cancelled)),
            "重启后驻留应由 cancel 收尾: {r:?}"
        );
        for (p, fill) in [(&d0, 7u8), (&d1, 8u8)] {
            let data = std::fs::read(p).unwrap();
            assert_eq!(data.len(), 30_000);
            assert!(data.iter().all(|&b| b == fill), "重启后内容错误");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
