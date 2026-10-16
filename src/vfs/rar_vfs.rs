//! RarVFS：RAR 格式（unrar crate 内嵌编译 UnRAR 源码，支持 RAR4/5、分卷）。
//! solid 包（`rar a -s`，主头 ROADF_SOLID）走后台解压会话 + 滑窗预取；
//! 非 solid 包（rar 默认）逐文件独立压缩，随机访问只解点击项（单文件按需）。

use super::{AccessMode, ArchiveVFS, SharedSession, VfsError, VirtualEntry, VirtualFile};
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
        let solid = Self::detect_solid(
            &first,
            candidates.first().map(|p| p.as_slice()),
        );
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
    /// （solid RAR 定位目标需逐条 skip，一次遍历解一批摊薄固定成本）；
    /// 目标之间的条目 skip 并按条目数 + 字节数上报进度
    fn try_batch(
        &self,
        pw: Option<&[u8]>,
        targets: &[(&VirtualEntry, &Path)],
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(u64, &str),
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
                        progress(0, "");
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
                        progress(size, "");
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
                        if skipped % 64 == 0 {
                            progress(skipped_bytes, &format!("跳过前置文件 {skipped}"));
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
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(u64, &str),
        file_done: &dyn Fn(usize, &Path),
    ) -> Result<(PassFlow, usize), VfsError> {
        let arch = match pw {
            Some(p) => Archive::with_password(&self.path, p),
            None => Archive::new(&self.path),
        };
        let mut open = arch.open_for_processing().map_err(|e| {
            VfsError::Archive(format!("打开 RAR 失败 [{}]: {e}", self.path.display()))
        })?;
        let mut landed = 0; // 本遍落地数
        let mut skipped: usize = 0;
        let mut skipped_bytes: u64 = 0;
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
                open = head.skip().map_err(|e| {
                    VfsError::Archive(format!("RAR 跳过条目 [{}] 失败（正在定位目标）: {e}", name))
                })?;
                skipped += 1;
                skipped_bytes += size;
                if skipped % 64 == 0 {
                    progress(skipped_bytes, &format!("跳过前置文件 {skipped}"));
                }
                continue;
            };
            if encrypted && pw.is_none() {
                return Err(VfsError::Password);
            }
            // 缓存已存在且尺寸一致：幂等跳过（候选重试/会话重启后已解出的不重做）
            if std::fs::metadata(&dest)
                .map(|m| m.len() == size)
                .unwrap_or(false)
            {
                if session.lock().unwrap().targets.remove(&name).is_some() {
                    file_done(ti, &dest); // 锁已释放
                    landed += 1;
                }
                open = head
                    .skip()
                    .map_err(|e| VfsError::Archive(format!("RAR 跳过条目 [{}] 失败: {e}", name)))?;
                continue;
            }
            // 解压到临时文件再改名（同 try_batch）；unrar 无逐字节回调：
            // 写文件前发 0、完成后发全量，UI 能区分"跳过 → 写文件"两阶段
            progress(0, "");
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
            if session.lock().unwrap().targets.remove(&name).is_some() {
                file_done(ti, &dest); // 锁已释放
                landed += 1;
            }
            progress(size, "");
            if session.lock().unwrap().targets.is_empty() {
                return Ok((PassFlow::Done, landed)); // 全部落地，提前收工
            }
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
    /// 前置文件。预算由 config `extract.prefetch_upstream_mb` 封顶
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
        progress: &dyn Fn(u64, &str),
    ) -> Result<(), VfsError> {
        self.extract_batch(&[(entry, dest)], cancel, progress, &|_| {})
    }

    fn extract_batch(
        &self,
        targets: &[(&VirtualEntry, &Path)],
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(u64, &str),
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
    /// （focus 回跳重启定位，上限 64 次防 UI 抖动死循环），外层规划轮在
    /// 一轮遍历尽后重读 targets（UI 新加的目标进下一轮；无人加则快照空
    /// 退出）。连续两轮零落地且 targets 仍非空 = 真的 entry 不存在（单轮
    /// 零落地可能是瞬态：遍历尽与 UI 加目标竞争），报错防死循环
    fn extract_session(
        &self,
        session: &SharedSession,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(u64, &str),
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
        // 名字 → list 下标（upstream 判定用；包内容不变，主循环外构建一次）
        let name_idx: std::collections::HashMap<String, usize> = self
            .list()
            .into_iter()
            .enumerate()
            .map(|(i, e)| (e.name, i))
            .collect();
        let mut stagnant = 0; // 连续零落地轮数（无进展保险）
        loop {
            // 持锁预检：取消 / 空闲退出（判空与置 alive=false 同锁，协议见
            // mod.rs）。顺带快照本轮 gen——UI 重定向同锁自增，零落地轮末
            // 比对可知"没进展"是否因中途换了目标
            let round_gen = {
                let mut st = session.lock().unwrap();
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    st.alive = false;
                    return Err(VfsError::Cancelled);
                }
                if st.targets.is_empty() {
                    st.alive = false;
                    return Ok(());
                }
                st.gen
            };
            let mut landed = 0;
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
        let tmp = super::tmp_path(&std::env::temp_dir().join("archview_pw_probe.tmp"));
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

    /// RAR 压缩是专有格式（无开源编码器），测试需借助 WinRAR 的 rar.exe：
    /// 探测 ARCHVIEW_TEST_RAR 环境变量 → Program Files 标准位置，找不到则跳过。
    /// 注：解压运行时用 unrar crate（内置编译），与 rar.exe 无关。
    fn rar_exe() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("ARCHVIEW_TEST_RAR") {
            let pb = PathBuf::from(p);
            if pb.exists() {
                return Some(pb);
            }
        }
        for base in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(pf) = std::env::var(base) {
                let p = PathBuf::from(pf).join("WinRAR").join("rar.exe");
                if p.exists() {
                    return Some(p);
                }
            }
        }
        None
    }

    #[test]
    fn list_and_extract() {
        let Some(rar) = rar_exe() else {
            eprintln!("跳过：未找到 WinRAR rar.exe（可设 ARCHVIEW_TEST_RAR 指定）");
            return;
        };
        let dir = std::env::temp_dir().join(format!("rar_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let payload = dir.join("a.bin");
        std::fs::write(&payload, vec![9u8; 50_000]).unwrap();
        let arch = dir.join("test.rar");
        let st = std::process::Command::new(&rar)
            .arg("a") // 添加
            .arg("-ep") // 不存路径（用 -ep 加子目录路径前缀手动拼）
            .arg(&arch)
            .arg(&payload)
            .output()
            .unwrap();
        assert!(st.status.success(), "rar.exe 打包失败");

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
            &|_, _| {},
        )
        .unwrap();
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len(), 50_000);
        assert!(data.iter().all(|&b| b == 9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 造包：3 × 30KB 不同填充字节的文件（f0=7, f1=8, f2=9，解出可校验）。
    /// solid=true 加 -s（主头 ROADF_SOLID）；rar 默认（无 -s）即非固态。
    /// 返回 (包路径, 临时目录)
    fn make_rar(tag: &str, solid: bool) -> Option<(PathBuf, PathBuf)> {
        let rar = rar_exe()?;
        let dir = std::env::temp_dir().join(format!("rar_mode_test_{}_{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let arch = dir.join(if solid { "solid.rar" } else { "plain.rar" });
        let mut cmd = std::process::Command::new(&rar);
        cmd.arg("a").arg("-ep"); // 包名必须紧跟命令，文件列表在后
        if solid {
            cmd.arg("-s");
        }
        cmd.arg(&arch);
        for (i, fill) in [7u8, 8, 9].into_iter().enumerate() {
            let p = dir.join(format!("f{i}.bin"));
            std::fs::write(&p, vec![fill; 30_000]).unwrap();
            cmd.arg(&p);
        }
        let st = cmd.output().unwrap();
        assert!(
            st.status.success(),
            "rar.exe 打包失败: {}",
            String::from_utf8_lossy(&st.stderr)
        );
        Some((arch, dir))
    }

    /// 非固态 RAR（rar 默认）→ AccessMode::SingleFile：随机访问只解点击项，
    /// 无上游免费区（skip 是廉价头跳转，整前缀预取等于白白解码）。中间条目
    /// 直接解压成功 = 真随机访问语义（不等价于顺序流定位）
    #[test]
    fn non_solid_rar_routes_single_file() {
        let Some((arch, dir)) = make_rar("ns", false) else {
            eprintln!("跳过：未找到 WinRAR rar.exe（可设 ARCHVIEW_TEST_RAR 指定）");
            return;
        };
        let vfs = RarVFS::open(&arch, None).unwrap();
        assert_eq!(vfs.access_mode(), AccessMode::SingleFile, "rar 默认非固态，应走单文件按需");
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
            &|_, _| {},
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
        let Some((arch, dir)) = make_rar("so", true) else {
            eprintln!("跳过：未找到 WinRAR rar.exe（可设 ARCHVIEW_TEST_RAR 指定）");
            return;
        };
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
            &|_, _| {},
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
}
