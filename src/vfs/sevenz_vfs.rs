//! SevenzVFS：7z 格式（solid 走慢路径会话，全独立 folder 走单文件按需）。
//! sevenz-rust 只支持顺序解压（固态块内 seek 需从块头重解），因此 open_entry 不支持，
//! 播放走后台解压到缓存目录 → 播放本地文件。

use super::{
    AccessMode, ArchiveVFS, ExtractPhase, SharedSession, VfsError, VirtualEntry, VirtualFile,
};
use sevenz_rust::{Password, SevenZReader};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct SevenzVFS {
    path: PathBuf,
    /// 内容加密条目解压用（加密头则在 open 阶段就需正确密码）
    password: Option<Password>,
    /// open 时从头解析并缓存的条目清单：list 不再重复解析 7z 头
    /// （旧实现 open/list 各解析一次，大包在网络盘上代价翻倍）
    entries: Vec<VirtualEntry>,
    /// 条目 → 所在 folder（与 entries 同下标）；folder → 该 folder 首个条目下标。
    /// 上游预取免费区 = [folder 首条目, 目标)（locate_start）
    folder_of: Vec<Option<usize>>,
    folder_first: std::collections::HashMap<usize, usize>,
    /// 包是否 solid：任一 folder 含 ≥2 条目（7z 的 solid = 多文件压进同一
    /// folder 顺序流；`7z a -ms=off` 每文件独立 folder，可 folder 直达随机
    /// 访问，无需滑窗会话）。决定 access_mode：
    /// solid = Solid（解压会话滑窗），全独立 = SingleFile（只解点击项）
    solid: bool,
}

/// sevenz-rust 错误 → VfsError（缺密码/密码错误统一映射 Password）
fn map_7z_err(e: sevenz_rust::Error) -> VfsError {
    match e {
        sevenz_rust::Error::PasswordRequired => VfsError::Password,
        sevenz_rust::Error::MaybeBadPassword(_) => VfsError::Password,
        e => VfsError::Archive(format!("7z 操作失败: {e}")),
    }
}

impl SevenzVFS {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self, VfsError> {
        // 打开即解析头（校验格式；加密头在无/错密码时这里就失败）
        let pw = password.map(Password::from).unwrap_or_else(Password::empty);
        let reader = SevenZReader::open(path, pw).map_err(map_7z_err)?;
        // 头读入后顺手缓存条目清单（list 直接 clone，不再重开重解析）
        let archive = reader.archive();
        // folder 级加密检测：AES coder（ID 06 F1 07 01）。sevenz-rust 不暴露
        // 条目级 content_methods，但 7z 的加密本就是 folder（固态块）粒度，
        // 条目加密 ⇔ 所在 folder 含 AES coder——由此 VirtualEntry.encrypted
        // 三格式全部可靠，密码探测不再需要对 7z 试解压盲探
        let enc_folder: Vec<bool> = archive
            .folders
            .iter()
            .map(|f| {
                f.coders.iter().any(|c| {
                    c.decompression_method_id() == sevenz_rust::SevenZMethod::ID_AES256SHA256
                })
            })
            .collect();
        let entries = archive
            .files
            .iter()
            .enumerate()
            .map(|(i, e)| VirtualEntry {
                name: e.name.clone(),
                size: e.size,
                is_dir: e.is_directory,
                // 无数据流的 0 字节条目不属于任何 folder，不算加密
                encrypted: archive
                    .stream_map
                    .file_folder_index
                    .get(i)
                    .and_then(|f| *f)
                    .is_some_and(|fi| enc_folder.get(fi).copied().unwrap_or(false)),
                // CRC 来自 7z 头（个别条目无 CRC 时 None，VarTidy 去重退化为尺寸判同）
                crc: if e.has_crc { Some(e.crc as u32) } else { None },
            })
            .collect();
        // folder 布局（locate_start 的上游预取免费区判据）：条目 → 所在 folder、
        // folder → 首个条目（首个条目的 locate_start 即其自身，免费区为空）
        let folder_of: Vec<Option<usize>> = archive.stream_map.file_folder_index.to_vec();
        let mut folder_first = std::collections::HashMap::new();
        let mut folder_size: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for (i, f) in folder_of.iter().enumerate() {
            if let Some(f) = f {
                folder_first.entry(*f).or_insert(i);
                *folder_size.entry(*f).or_insert(0) += 1;
            }
        }
        // solid 判定：任一 folder 压了 ≥2 条目即顺序流；全部单条 folder
        // （-ms=off）则每文件独立可直达，与"非 solid RAR"同理走单文件按需
        let solid = folder_size.values().any(|&n| n >= 2);
        Ok(Self {
            path: path.to_path_buf(),
            password: password.map(Password::from),
            entries,
            folder_of,
            folder_first,
            solid,
        })
    }

    fn reader(&self) -> Result<SevenZReader<std::fs::File>, VfsError> {
        let pw = self.password.clone().unwrap_or_else(Password::empty);
        SevenZReader::open(&self.path, pw).map_err(map_7z_err)
    }

    /// 批边界真暂停：整会话目标解净时在条目边界阻塞（BlockDecoder 活着、
    /// 解码位置停在当前条目（cur_idx），这是顺序浏览"零前缀续解"的关键），
    /// 直至 UI 重定向写入新目标或 cancel。分流精判（2026-11-28 修复：
    /// 原判定"targets 含任何非本 folder 条目即中断"过粗——下游收集允许
    /// 跨 folder，新批尾部常带下一个 folder 的头几项，连带把本 folder
    /// 打断回外层重排，focus folder 从 pack 头重解前缀百秒级）：
    /// 返回 Ok(true) = 本 folder 解码流身前（idx > cur_idx）仍有目标，
    /// 顺流续解（流过即落地），folder 解完后外层按剩余 targets 重排进入
    /// 其余 folder，绝不因跨 folder 批重解本 folder 前缀；
    /// Ok(false) = 本 folder 身前无目标（目标全在解码流身后——回看抢道，
    /// 或全在别的 folder）→ 中断本 folder 回外层重排（focus folder 提前，
    /// 跨 folder 直达）；Err = cancel。睡眠分片检查，不持锁睡眠，唤醒
    /// 延迟 ≤ 一个分片
    fn pause_at_batch_boundary(
        &self,
        session: &SharedSession,
        cancel: &std::sync::atomic::AtomicBool,
        folder: usize,
        cur_idx: usize,
    ) -> Result<bool, sevenz_rust::Error> {
        use std::sync::atomic::Ordering;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(sevenz_rust::Error::other("cancelled"));
            }
            let (has_tgt, ahead_in_folder) = {
                let st = session.lock().unwrap();
                (
                    !st.targets.is_empty(),
                    // 实时判定（每次唤醒重读 targets）：本 folder 解码流身前
                    // 是否还有目标——有则顺流可及，中断反而造成前缀重解
                    st.targets.values().any(|(idx, _)| {
                        *idx > cur_idx
                            && self.folder_of.get(*idx).copied().flatten() == Some(folder)
                    }),
                )
            };
            if has_tgt {
                tracing::debug!(
                    "[会话] 批边界唤醒判定: folder{folder} 身前（idx>{cur_idx}）有目标={ahead_in_folder} → {}",
                    if ahead_in_folder { "顺流续解" } else { "中断回外层重排" }
                );
                return Ok(ahead_in_folder);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// 纯元数据段总量求和（entries×folder_of×targets/focus，无 IO、无副作用、
    /// 不触碰 targets/收集窗口/解码流）：规划分母与唤醒臂骨架同源同款。
    /// 只计 folders 内 [from_idx, 该 folder 最后一个目标]（解码实际停止点，
    /// 之后与之前的条目不会流经/已流经）；归类必须与解码回调同源——先判
    /// 是否目标（tgt_idx），再按 focus 分段（focus 指向的条目落地后已从
    /// targets 移除而 UI 未重定向时，focus 陈旧指向非目标条目——若按
    /// i == focus_idx 先判 Target，Target 段被发骨架却永远无写入，bar 卡
    /// 0% 且 Downstream 串量；先判目标则两侧逐条对应）
    fn segment_totals(
        &self,
        folders: &std::collections::BTreeSet<usize>,
        focus_idx: usize,
        tgt_idx: &std::collections::HashSet<usize>,
        from_idx: usize,
    ) -> [(u64, u64); ExtractPhase::COUNT] {
        let mut totals = [(0u64, 0u64); ExtractPhase::COUNT];
        let Some(&last) = tgt_idx.iter().max() else {
            return totals;
        };
        // folder → 该 folder 内最后一个目标下标（解码的实际停止点）
        let mut stop: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for &i in tgt_idx {
            if let Some(Some(f)) = self.folder_of.get(i) {
                stop.entry(*f)
                    .and_modify(|s| *s = (*s).max(i))
                    .or_insert(i);
            }
        }
        for i in from_idx..=last {
            if let Some(Some(f)) = self.folder_of.get(i).copied() {
                if !folders.contains(&f) {
                    continue;
                }
                let Some(&s) = stop.get(&f) else {
                    continue;
                };
                if i > s {
                    continue;
                }
                let size = self.entries[i].size;
                let ph = if tgt_idx.contains(&i) {
                    match i.cmp(&focus_idx) {
                        std::cmp::Ordering::Equal => ExtractPhase::Target,
                        std::cmp::Ordering::Less => ExtractPhase::Upstream,
                        std::cmp::Ordering::Greater => ExtractPhase::Downstream,
                    }
                } else if i < focus_idx {
                    ExtractPhase::SkipUp
                } else {
                    ExtractPhase::Downstream
                };
                totals[ph as usize].1 += size;
            }
        }
        totals
    }

    /// 唤醒臂骨架（批边界驻留被新批唤醒、顺流续解前调用）：醒来重读纪律——
    /// 暂停前快照的 focus/targets 禁沿用。新鲜 focus/targets 纯元数据求和
    /// （segment_totals，规划同源同款；不动收集窗口/targets/解码流，仅上报
    /// 口径），from=cur_idx+1（唤醒点条目正在消费，不进分母）。返回
    /// (新鲜 focus, 各段总量)，调用方换 focus_idx + 发开工骨架
    fn wake_skeleton(
        &self,
        session: &SharedSession,
        folder: usize,
        cur_idx: usize,
    ) -> (usize, [(u64, u64); ExtractPhase::COUNT]) {
        let mut st = session.lock().unwrap();
        // 本轮解码 = 驻留唤醒顺流续解（解码器未重启，接着上次位置流）：
        // UI 覆盖层标题据此区分"续解（无前缀重解）"与"folder 直达重解"
        st.resume = true;
        let fresh_focus = st
            .focus
            .unwrap_or_else(|| st.targets.values().map(|(i, _)| *i).min().unwrap_or(0));
        let wake_tgt: std::collections::HashSet<usize> =
            st.targets.values().map(|(i, _)| *i).collect();
        drop(st); // 求和无锁依赖，先放锁
        let mut folders = std::collections::BTreeSet::new();
        folders.insert(folder);
        (
            fresh_focus,
            self.segment_totals(&folders, fresh_focus, &wake_tgt, cur_idx + 1),
        )
    }
}

impl ArchiveVFS for SevenzVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        self.entries.clone()
    }

    fn open_entry(&self, _entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        Err(VfsError::Unsupported(
            "7z 固态格式不支持流式读取，请走后台解压路径".into(),
        ))
    }

    fn access_mode(&self) -> AccessMode {
        if self.solid {
            AccessMode::Solid
        } else {
            // 全独立 folder（-ms=off）：BlockDecoder 按 folder 头 pack 偏移
            // 直达，随机访问只解点击项所在 folder = 单文件按需，不付滑窗/
            // 预取/重定向成本
            AccessMode::SingleFile
        }
    }

    /// 上游预取免费区 = 目标所在 folder 的 [首条目, 目标)：定位目标时
    /// 这段数据必然被顺序解掉丢弃（BlockDecoder 从 folder 头解码到目标），
    /// 顺带写盘零解码增量。跨 folder 不免费（只 seek 不解压）——
    /// 上游预取不得超出 folder 边界（用户配置的预算可能大于 folder 前缀，实取以边界为准）。
    /// **仅 solid 7z 有意义**：全独立 folder 的包 BlockDecoder 直达本 folder
    /// 首条目即目标，免费区为空，返回 None 不做上游预取
    fn locate_start(&self, entry_idx: usize) -> Option<usize> {
        if !self.solid {
            return None;
        }
        let folder = self.folder_of.get(entry_idx).copied().flatten()?;
        self.folder_first.get(&folder).copied()
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

    /// 批量解压 + folder 直达。
    ///
    /// 7z 的 folder（固态块）彼此独立：每个 folder 的 pack 流偏移记录在头里，
    /// `BlockDecoder` 构造时会直接 seek 过去解码，**无需解压前面的任何 folder**。
    /// 旧实现用 `for_each_entries` 从 folder 0 开始顺序解压全部前置数据——
    /// 56G solid 包里目标即使排在第 400 个条目，前面也可能压着几十 GB 的
    /// 前置 folder（约 10 分钟级、CPU 持续满载，表象酷似死循环 bug）。
    /// 现按目标所在 folder 直接定位，只顺序解压目标 folder 内目标之前的少量数据；
    /// 批量目标按 folder 升序逐个处理（跨 folder 的预取只 seek 不解压）。
    fn extract_batch(
        &self,
        targets: &[(&VirtualEntry, &Path)],
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
        file_done: &dyn Fn(usize),
    ) -> Result<(), VfsError> {
        use std::collections::{BTreeSet, HashMap};
        use std::sync::atomic::Ordering;

        // 解析头一次（一次顺序读）：条目清单与 folder 布局都在里面
        let reader = self.reader()?;
        let archive = reader.archive();
        let pw = self
            .password
            .clone()
            .unwrap_or_else(Password::empty)
            .to_vec();

        // 目标名 → 批内下标 / 所在 folder 号
        let mut remaining: HashMap<&str, usize> = targets
            .iter()
            .enumerate()
            .map(|(i, (e, _))| (e.name.as_str(), i))
            .collect();
        let mut name_folder: HashMap<&str, usize> = HashMap::new();
        let mut folders: BTreeSet<usize> = BTreeSet::new();
        for (fi, f) in archive.files.iter().enumerate() {
            if remaining.contains_key(f.name.as_str()) {
                match archive.stream_map.file_folder_index[fi] {
                    Some(folder) => {
                        name_folder.insert(f.name.as_str(), folder);
                        folders.insert(folder);
                    }
                    // 无数据流的条目（0 字节）：不在任何 folder，落地空文件即完成
                    None => {
                        let i = remaining.remove(f.name.as_str()).unwrap();
                        std::fs::write(targets[i].1, b"").map_err(|e| {
                            VfsError::Archive(format!(
                                "写入空文件失败 [{}]: {e}",
                                targets[i].1.display()
                            ))
                        })?;
                        file_done(i);
                    }
                }
            }
        }

        for fi in folders {
            if remaining.is_empty() {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(VfsError::Cancelled);
            }
            // 每个 folder 独立文件句柄：BlockDecoder 自行 seek 到该 folder 的
            // pack 偏移，前面的 folder 完全不解压（旧实现慢 10 分钟的根源）
            let mut src = std::fs::File::open(&self.path).map_err(VfsError::Io)?;
            let dec = sevenz_rust::BlockDecoder::new(fi, archive, &pw, &mut src);
            // 本 folder 内待解目标数：解完即停，folder 内后续数据也不再解压
            let mut left_in_folder = name_folder.values().filter(|&&f| f == fi).count();
            let mut skipped: usize = 0;
            let mut skipped_bytes: u64 = 0;
            let r = dec.for_each_entries(&mut |e, data| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(sevenz_rust::Error::other("cancelled"));
                }
                if let Some(&ti) = remaining.get(e.name.as_str()) {
                    let dest = targets[ti].1;
                    left_in_folder -= 1;
                    // 静态批不知道批内目标的物理次序（UI 传入顺序 = 点击项
                    // 优先），一律按目标写入段上报
                    write_entry_data(data, e.size, dest, ExtractPhase::Target, 0, e.size, progress)?;
                    remaining.remove(e.name.as_str());
                    file_done(ti);
                    // 本 folder 目标全部解出：停止迭代（后续 folder 由外层循环处理）
                    Ok(left_in_folder > 0)
                } else {
                    // folder 内非目标条目仍需顺序读完丢弃：solid 块内数据首尾
                    // 相接，crate 的 BoundedReader 不带 drop-skip，不消费数据
                    // 会导致后续 entry 读取整体错位、CRC 校验失败
                    // （solid_7z_extract_later_entry 回归测试覆盖此路径）。
                    // 计数 sink 就地丢弃 + 按 4MB 粒度上报跳过段进度
                    // （字节可见"在动"，不像卡死）；读侧错误保留 io 形态
                    // （坏密码路径同上）。静态批无 list 下标，跳过段无法算
                    // 定比总量（total=0 = 无分母，UI 显示活动指示）
                    let mut sink = PhaseSink::new(ExtractPhase::SkipUp, 0, 0, progress);
                    std::io::copy(data, &mut sink).map_err(sevenz_rust::Error::io)?;
                    skipped += 1;
                    skipped_bytes += sink.n;
                    if skipped.is_multiple_of(64) {
                        progress(ExtractPhase::SkipUp, skipped_bytes, 0);
                    }
                    Ok(true)
                }
            });
            r.map_err(|e| {
                // 取消：静默（调用方主动放弃）
                if cancel.load(Ordering::Relaxed) {
                    return VfsError::Cancelled;
                }
                // 密码错误单独识别（UI 弹密码框重试），其余带上完整上下文
                let mapped = map_7z_err(e);
                if matches!(mapped, VfsError::Password) {
                    mapped
                } else {
                    VfsError::Archive(format!(
                        "7z 解压失败: 压缩包 [{}] 条目 [{}]: {mapped}",
                        self.path.display(),
                        remaining.keys().copied().collect::<Vec<_>>().join(", ")
                    ))
                }
            })?;
        }
        if !remaining.is_empty() {
            let names: Vec<&str> = remaining.keys().copied().collect();
            return Err(VfsError::Archive(format!(
                "entry 不存在: {}",
                names.join(", ")
            )));
        }
        Ok(())
    }

    /// 会话化解压（协议见 trait 注释）：外层规划循环 + folder 直达 +
    /// focus 漂移中断，跳转不再取消解码流。
    /// - 规划（持锁快照）：目标按 folder_of 分组（缓存自 open，无需扫包），
    ///   无 folder（0 字节）条目直接落地空文件；focus 所在 folder 提到
    ///   解码序最前（BTreeSet 升序底序，顺序读盘友好）
    /// - folder 解码：BlockDecoder 直达，回调内动态查 targets（UI 随时增删）；
    ///   本 folder 目标解净且整会话无目标 → 批边界真暂停（驻留等新目标，
    ///   解码位置停在条目边界）；本 folder 解净但其它 folder 仍有目标 →
    ///   停 folder 转下一个；非目标条目顺序消费丢弃（solid 流不可跳，
    ///   错位必 CRC 失败）
    /// - 漂移中断：focus 移到其它 folder 且仍待解 → Ok(false) 中断当前
    ///   folder 回外层重排（focus folder 最先）——被中断 folder 之后从头
    ///   重解（自愈，成本与"取消重启"旧路径持平）
    /// - 同 folder 回跳上游（目标在本趟已越过位置）：本趟解到尾不会命中，
    ///   下轮规划重排该 folder 从头重解补齐（自愈）
    /// - 会话生命周期：真暂停后线程驻留不退出（规划期空目标同样驻留等待），
    ///   退出只剩 cancel（切包）与出错两条路；UI 侧批完成由 pending 清空
    ///   自行判定
    fn extract_session(
        &self,
        session: &SharedSession,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
        file_done: &dyn Fn(usize, &Path),
    ) -> Result<(), VfsError> {
        use std::collections::BTreeSet;
        use std::sync::atomic::Ordering;

        let reader = self.reader()?;
        let archive = reader.archive();
        let pw = self
            .password
            .clone()
            .unwrap_or_else(Password::empty)
            .to_vec();
        // 条目名 → 下标：非目标条目按其下标与 focus 的相对位置分入
        // SkipUp（focus 之前）/ Downstream（focus 之后）段
        let idx_of: std::collections::HashMap<&str, usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.as_str(), i))
            .collect();

        loop {
            // ---- 驻留等待（真暂停·规划期）：批解净后线程不退出，原地等
            // UI 重定向写入新目标——顺序浏览续解、跳转续跑都不再付出
            // "重新找 folder 起点 + 重解前缀"的代价（原空闲退出路径已被
            // 驻留取代；会话退出只剩 cancel 切包与出错两条路，UI 侧"批
            // 完成"由 pending 清空自行判定，不依赖线程退出）
            loop {
                if cancel.load(Ordering::Relaxed) {
                    super::session_exit(session);
                    return Err(VfsError::Cancelled);
                }
                let empty = session.lock().unwrap().targets.is_empty();
                if !empty {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // ---- 规划：持锁快照目标集 → folder 分组 + focus 优先 ----
            // mut：驻留唤醒顺流续解时按新批 focus 重置（唤醒臂 wake_skeleton）
                let (folders, focus_folder, zero_streams, round_gen, mut focus_idx, tgt_idx) = {
                // 取消与置 alive=false 同一次持锁（协议：与 UI 加目标互斥）
                let mut st = session.lock().unwrap();
                if cancel.load(Ordering::Relaxed) {
                    st.alive = false;
                    return Err(VfsError::Cancelled);
                }
                // 规划（重）开工 = BlockDecoder 从 folder 起点直达顺序解：
                // 续解标志复位（唤醒续解臂会再置 true，UI 据此切换标题文案）
                st.resume = false;
                // targets 判空退出已由上方驻留等待取代：进入规划即保证非空
                // （UI 重定向的 clear+重写同锁原子，期间不会观察到空集）
                // 本轮规划代数：UI 重定向在同一把锁内改 targets 并自增 gen，
                // 两者快照同锁保证一致（gen 变了 ⟺ targets 换过）
                let round_gen = st.gen;
                let mut folders: BTreeSet<usize> = BTreeSet::new();
                let mut zero: Vec<(String, usize, PathBuf)> = Vec::new();
                for (name, (idx, dest)) in &st.targets {
                    match self.folder_of.get(*idx).copied().flatten() {
                        Some(f) => {
                            folders.insert(f);
                        }
                        // 无数据流的 0 字节条目：不在任何 folder，落地空文件即完成
                        None => zero.push((name.clone(), *idx, dest.clone())),
                    }
                }
                // focus 仍待解且指向别的 folder → 该 folder 优先（漂移抢道）
                let focus_folder = st
                    .focus
                    .filter(|fi| {
                        self.entries
                            .get(*fi)
                            .is_some_and(|e| st.targets.contains_key(&e.name))
                    })
                    .and_then(|fi| self.folder_of.get(fi).copied().flatten());
                // 分段锚点：focus 指针（无 focus = 最小目标下标）；目标下标集合
                // （含 0 字节无流条目，totals 只按有流部分累计）
                let focus_idx = st.focus.unwrap_or_else(|| {
                    st.targets.values().map(|(i, _)| *i).min().unwrap_or(0)
                });
                let tgt_idx: std::collections::HashSet<usize> =
                    st.targets.values().map(|(i, _)| *i).collect();
                (folders, focus_folder, zero, round_gen, focus_idx, tgt_idx)
            };
            // 锁外统计本轮各段定比总量（entries/folder_of 为 open 缓存，无 IO）：
            // 规划分母与唤醒臂骨架同源同款（纯元数据求和，见 segment_totals）
            let totals = self.segment_totals(&folders, focus_idx, &tgt_idx, 0);
            // 开工协议：发 Locate(0,0)（总量 0 → UI 显示定位 spinner）+
            // 各非零段 (0, total)（UI 画空 bar 骨架）
            progress(ExtractPhase::Locate, 0, 0);
            for ph in [
                ExtractPhase::SkipUp,
                ExtractPhase::Upstream,
                ExtractPhase::Target,
                ExtractPhase::Downstream,
            ] {
                let t = totals[ph as usize].1;
                if t > 0 {
                    progress(ph, 0, t);
                }
            }
            let mut landed: usize = 0; // 本轮落地数（无进展保险的判据）
                                       // 无 folder 条目：锁外写空文件 → 锁内 remove 成功才算落地
            for (name, idx, dest) in zero_streams {
                if let Err(e) = std::fs::write(&dest, b"") {
                    super::session_exit(session);
                    return Err(VfsError::Archive(format!(
                        "写入空文件失败 [{}]: {e}",
                        dest.display()
                    )));
                }
                if session.lock().unwrap().targets.remove(&name).is_some() {
                    file_done(idx, &dest); // 锁已释放
                    landed += 1;
                }
            }
            // 解码序：升序底序，focus folder 提到最前
            let mut order: Vec<usize> = folders.into_iter().collect();
            if let Some(ff) = focus_folder {
                if let Some(pos) = order.iter().position(|&f| f == ff) {
                    order.remove(pos);
                    order.insert(0, ff);
                }
            }
            tracing::debug!(
                "[会话] 规划（gen {round_gen}）: 解码 folder 序={order:?}，focus_folder={focus_folder:?}，目标 {} 项",
                tgt_idx.len()
            );

            // 轮级段游标（跨 folder 累计，条目相对 focus 分段，与解码序无关）
            // mut：真暂停唤醒续解时清零（新批未经重规划，旧总量失效，UI 走
            // 活动指示，待下一轮规划恢复定比）
            let (mut skip_cur, mut up_cur, mut down_cur) = (0u64, 0u64, 0u64);
            let (mut skip_total, mut up_total, mut down_total) = (
                totals[ExtractPhase::SkipUp as usize].1,
                totals[ExtractPhase::Upstream as usize].1,
                totals[ExtractPhase::Downstream as usize].1,
            );

            let mut interrupted = false;
            for &fi in &order {
                if cancel.load(Ordering::Relaxed) {
                    super::session_exit(session);
                    return Err(VfsError::Cancelled);
                }
                // 该 folder 是否仍有目标（上轮部分解出后可能已清空）
                let has_target = session
                    .lock()
                    .unwrap()
                    .targets
                    .values()
                    .any(|(idx, _)| self.folder_of.get(*idx) == Some(&Some(fi)));
                if !has_target {
                    continue;
                }
                // folder 独立文件句柄：BlockDecoder 自行 seek 到该 folder 的
                // pack 偏移，前置 folder 只 seek 不解压
                let mut src = match std::fs::File::open(&self.path) {
                    Ok(f) => f,
                    Err(e) => {
                        super::session_exit(session);
                        return Err(VfsError::Io(e));
                    }
                };
                let dec = sevenz_rust::BlockDecoder::new(fi, archive, &pw, &mut src);
                let mut seg_since_report: usize = 0; // 距上次段进度节流上报的条目数
                let r = dec.for_each_entries(&mut |e, data| {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(sevenz_rust::Error::other("cancelled"));
                    }
                    let ename = e.name.as_str(); // 日志格式串用（不支持字段访问）
                    // 持锁三元组快照：本条目目标 / focus 漂移 / 本 folder 已解净
                    let (tgt, drift, folder_clear) = {
                        let st = session.lock().unwrap();
                        let tgt = st.targets.get(e.name.as_str()).cloned();
                        let drift = st.focus.is_some_and(|f_idx| match self.entries.get(f_idx) {
                            // focus 仍待解且在别的 folder：抢道
                            Some(fe) if st.targets.contains_key(&fe.name) => {
                                self.folder_of.get(f_idx) != Some(&Some(fi))
                            }
                            _ => false,
                        });
                        let folder_clear = !st
                            .targets
                            .values()
                            .any(|(idx, _)| self.folder_of.get(*idx) == Some(&Some(fi)));
                        (tgt, drift, folder_clear)
                    };
                    if drift {
                        tracing::debug!(
                            "[会话] 条目[{ename}] 检测到 focus 漂移抢道（focus 在别的 folder）→ 中断本 folder"
                        );
                        interrupted = true;
                        return Ok(false); // 中断本 folder，外层重排抢道 focus
                    }
                    if let Some((idx, dest)) = tgt {
                        // 目标按其下标与 focus 的相对位置分入 Target/Upstream/
                        // Downstream 段：写前 (base, seg_total) 起点上报、写中
                        // 4MB 粒度、写完落地后推进段游标再报一次
                        let (phase, base, seg_total) = if idx == focus_idx {
                            (ExtractPhase::Target, 0u64, e.size)
                        } else if idx < focus_idx {
                            (ExtractPhase::Upstream, up_cur, up_total)
                        } else {
                            (ExtractPhase::Downstream, down_cur, down_total)
                        };
                        write_entry_data(data, e.size, &dest, phase, base, seg_total, progress)?;
                        match phase {
                            ExtractPhase::Upstream => {
                                up_cur += e.size;
                                progress(phase, up_cur, up_total);
                            }
                            ExtractPhase::Downstream => {
                                down_cur += e.size;
                                progress(phase, down_cur, down_total);
                            }
                            _ => {} // Target 由 write_entry_data 报满
                        }
                        let hit = {
                            let mut st = session.lock().unwrap();
                            st.skip_note = None; // 目标落地，跳过提示过期
                            st.targets.remove(e.name.as_str()).is_some()
                        };
                        if hit {
                            tracing::debug!("[会话] 落地 [{ename}]（idx {idx}）");
                            file_done(idx, &dest); // 锁已释放
                            landed += 1;
                        }
                        // 本 folder 目标解净：若整会话也无目标 → 批边界真暂停
                        // （驻留等新目标，解码位置停在当前条目）；唤醒续解时按
                        // 新批 focus/targets 重算段骨架（唤醒臂 wake_skeleton）
                        if folder_clear {
                            tracing::debug!(
                                "[会话] 条目[{ename}]（idx {idx}）目标落地后 folder{fi} 清 → 批边界驻留等待"
                            );
                            match self.pause_at_batch_boundary(session, cancel, fi, idx) {
                                Ok(true) => {
                                    tracing::debug!(
                                        "[会话] 驻留唤醒（顺流）：当前条目[{ename}] 已作为目标落地，继续解后续条目"
                                    );
                                    // 醒来重读纪律：新鲜 focus/targets 重算段骨架
                                    // （wake_skeleton，纯元数据求和，规划同源同款；
                                    // 不动收集窗口/targets/解码流，仅上报口径），
                                    // 后续归段也换新鲜 focus
                                    let (fresh_focus, wake_totals) =
                                        self.wake_skeleton(session, fi, idx);
                                    focus_idx = fresh_focus;
                                    skip_cur = 0;
                                    up_cur = 0;
                                    down_cur = 0;
                                    skip_total = wake_totals[ExtractPhase::SkipUp as usize].1;
                                    up_total = wake_totals[ExtractPhase::Upstream as usize].1;
                                    down_total = wake_totals[ExtractPhase::Downstream as usize].1;
                                    // 开工协议同款：Locate(0,0) + 各非零段 (0,total)
                                    progress(ExtractPhase::Locate, 0, 0);
                                    for ph in [
                                        ExtractPhase::SkipUp,
                                        ExtractPhase::Upstream,
                                        ExtractPhase::Target,
                                        ExtractPhase::Downstream,
                                    ] {
                                        let t = wake_totals[ph as usize].1;
                                        if t > 0 {
                                            progress(ph, 0, t);
                                        }
                                    }
                                    return Ok(true); // 新目标在本 folder：顺流续解
                                }
                                Ok(false) => {
                                    interrupted = true;
                                    return Ok(false); // 新目标在别的 folder：回外层重排
                                }
                                Err(_) => return Err(sevenz_rust::Error::other("cancelled")),
                            }
                        }
                        Ok(true)
                    } else {
                        let self_idx = idx_of.get(e.name.as_str()).copied().unwrap_or(usize::MAX);
                        if folder_clear {
                            // 驻留等待 = 无消费动作，跳过提示过期清空
                            session.lock().unwrap().skip_note = None;
                            tracing::debug!(
                                "[会话] 条目[{ename}]（idx {self_idx}）非目标且 folder{fi} 目标清 → 批边界驻留等待"
                            );
                            // 批边界真暂停（语义同 tgt 分支）：整会话解净时
                            // 驻留等新目标而非停 folder——原有行为（停 folder
                            // 回外层）会让线程空闲退出、下次点击重解 folder
                            // 前缀；唤醒分流与新目标位置同 tgt 分支
                            match self.pause_at_batch_boundary(session, cancel, fi, self_idx) {
                                Ok(true) => {
                                    // 醒来重读纪律：新鲜 focus/targets 重算段骨架
                                    // （wake_skeleton，纯元数据求和，规划同源同款；
                                    // 不动收集窗口/targets/解码流，仅上报口径），
                                    // 后续归段也换新鲜 focus
                                    let (fresh_focus, wake_totals) =
                                        self.wake_skeleton(session, fi, self_idx);
                                    focus_idx = fresh_focus;
                                    skip_cur = 0;
                                    up_cur = 0;
                                    down_cur = 0;
                                    skip_total = wake_totals[ExtractPhase::SkipUp as usize].1;
                                    up_total = wake_totals[ExtractPhase::Upstream as usize].1;
                                    down_total = wake_totals[ExtractPhase::Downstream as usize].1;
                                    progress(ExtractPhase::Locate, 0, 0);
                                    for ph in [
                                        ExtractPhase::SkipUp,
                                        ExtractPhase::Upstream,
                                        ExtractPhase::Target,
                                        ExtractPhase::Downstream,
                                    ] {
                                        let t = wake_totals[ph as usize].1;
                                        if t > 0 {
                                            progress(ph, 0, t);
                                        }
                                    }
                                    // 唤醒点条目可能已被重定向收为新目标：本条目
                                    // 数据未消费，改判目标当场落地。若仍按非目标
                                    // 丢弃，它挂在 targets 里够不着 → folder 解完
                                    // 触发自愈前缀重解（实测一项挂起 106 秒转圈，
                                    // 2026-11-28 日志 ⚠ 桩实锤后修复）
                                    // 禁止把 session.lock() 直接写进 if let
                                    // scrutinee：2021 edition 下守卫临时值
                                    // 存活到整个 if-let 块结束，块内
                                    // targets.remove 再次加锁 = 同线程
                                    // Mutex 自死锁（实测：改判落地文件
                                    // 完成后线程冻结、零日志零自愈）
                                    let wake_tgt = session
                                        .lock()
                                        .unwrap()
                                        .targets
                                        .get(e.name.as_str())
                                        .cloned();
                                    if let Some((idx, dest)) = wake_tgt {
                                        // 归段按新鲜 focus 的相对位置（唤醒臂已换
                                        // focus 并重算段骨架，与规划侧同源）
                                        let (phase, base, seg_total) = if idx == focus_idx {
                                            (ExtractPhase::Target, 0u64, e.size)
                                        } else if idx < focus_idx {
                                            (ExtractPhase::Upstream, up_cur, up_total)
                                        } else {
                                            (ExtractPhase::Downstream, down_cur, down_total)
                                        };
                                        write_entry_data(
                                            data,
                                            e.size,
                                            &dest,
                                            phase,
                                            base,
                                            seg_total,
                                            progress,
                                        )?;
                                        match phase {
                                            ExtractPhase::Upstream => {
                                                up_cur += e.size;
                                                progress(phase, up_cur, up_total);
                                            }
                                            ExtractPhase::Downstream => {
                                                down_cur += e.size;
                                                progress(phase, down_cur, down_total);
                                            }
                                            _ => {}
                                        }
                                        let hit = {
                                            let mut st = session.lock().unwrap();
                                            st.skip_note = None; // 〔唤醒改判〕落地，跳过提示过期
                                            st.targets.remove(e.name.as_str()).is_some()
                                        };
                                        if hit {
                                            tracing::debug!(
                                                "[会话] 落地 [{ename}]（idx {idx}）〔唤醒改判〕"
                                            );
                                            file_done(idx, &dest); // 锁已释放
                                            landed += 1;
                                        }
                                        return Ok(true); // 顺流续解后续条目
                                    }
                                    tracing::debug!(
                                        "[会话] 驻留唤醒（顺流）：当前条目[{ename}] 非目标，消费丢弃"
                                    );
                                    // 新目标在本 folder：顺流续解，本条目按非目标消费
                                }
                                Ok(false) => {
                                    interrupted = true;
                                    return Ok(false);
                                }
                                Err(_) => return Err(sevenz_rust::Error::other("cancelled")),
                            }
                        }
                        // 跳过提示：消费边界短锁刷新（写完即放，绝不跨
                        // io::copy 持锁）——UI 定位转圈期显示正在跳过的条目
                        // （大视频跳过 = 真解码，是定位耗时的主力）
                        session.lock().unwrap().skip_note =
                            Some((self_idx, e.name.clone(), e.size));
                        // 非目标条目顺序消费丢弃（solid 流不可跳，同 extract_batch），
                        // 按条目下标与 focus 相对位置分入 SkipUp/Downstream 段
                        let is_down = idx_of
                            .get(e.name.as_str())
                            .is_some_and(|&i| i >= focus_idx);
                        if is_down {
                            let mut sink = PhaseSink::new(
                                ExtractPhase::Downstream,
                                down_cur,
                                down_total,
                                progress,
                            );
                            std::io::copy(data, &mut sink).map_err(sevenz_rust::Error::io)?;
                            down_cur += sink.n;
                        } else {
                            let mut sink = PhaseSink::new(
                                ExtractPhase::SkipUp,
                                skip_cur,
                                skip_total,
                                progress,
                            );
                            std::io::copy(data, &mut sink).map_err(sevenz_rust::Error::io)?;
                            skip_cur += sink.n;
                        }
                        seg_since_report += 1;
                        if seg_since_report.is_multiple_of(64) {
                            if is_down {
                                progress(ExtractPhase::Downstream, down_cur, down_total);
                            } else {
                                progress(ExtractPhase::SkipUp, skip_cur, skip_total);
                            }
                        }
                        Ok(true)
                    }
                });
                tracing::debug!(
                    "[会话] folder{fi} 遍历结束（r={}，interrupted={interrupted}，landed={landed}）",
                    r.is_err()
                );
                if let Err(e) = r {
                    super::session_exit(session);
                    if cancel.load(Ordering::Relaxed) {
                        return Err(VfsError::Cancelled);
                    }
                    let mapped = map_7z_err(e);
                    return Err(match mapped {
                        VfsError::Password => mapped,
                        _ => {
                            let names = session
                                .lock()
                                .unwrap()
                                .targets
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ");
                            VfsError::Archive(format!(
                                "7z 解压失败: 压缩包 [{}] 条目 [{}]: {mapped}",
                                self.path.display(),
                                names
                            ))
                        }
                    });
                }
                if interrupted {
                    tracing::debug!("[会话] folder{fi} 中断回外层重排（漂移抢道或跨 folder 唤醒）");
                    break; // 回外层重新规划（focus folder 提前）
                }
            }
            // 无进展保险：一轮正常跑完却零落地——目标永远解不出（异常路径），
            // 报错退出防死循环（漂移中断轮允许零落地，下一轮会解 focus）。
            // 例外：本轮中途 UI 重定向过（gen 已变）——如跳到已落地的缓存
            // 项，focus 不在 targets、各 folder 均已解净，全程零落地属正常，
            // 重新规划续跑新目标而非误报
            if !interrupted && landed == 0 {
                let cur_gen = session.lock().unwrap().gen;
                if cur_gen != round_gen {
                    tracing::debug!(
                        "7z 会话本轮零落地但检测到重定向（gen {round_gen}→{cur_gen}），重新规划续跑"
                    );
                    continue;
                }
                super::session_exit(session);
                return Err(VfsError::Archive(
                    "7z 会话无进展: 目标条目未出现在 folder 解码流中".into(),
                ));
            }
        }
    }

    /// 密码校验：7z 头不存储密码校验值，唯一可靠的判据是真解压 + CRC——
    /// 用 BlockDecoder folder 直达目标条目，解压到内存 sink（零落盘），
    /// 错密码必致 CRC 失败，sevenz-rust 在密码非空时将其转报
    /// MaybeBadPassword → Password。目标之前的同 folder 条目必须顺序消费
    /// （solid 流不可跳），故探测目标应选最小加密条目
    fn verify_password(
        &self,
        entry: &VirtualEntry,
        password: Option<&str>,
    ) -> Result<(), VfsError> {
        let pw = password.map(Password::from).unwrap_or_else(Password::empty);
        let reader = SevenZReader::open(&self.path, pw.clone()).map_err(map_7z_err)?;
        let archive = reader.archive();
        let fi = archive
            .files
            .iter()
            .position(|f| f.name == entry.name)
            .ok_or_else(|| VfsError::Archive(format!("entry 不存在: {}", entry.name)))?;
        let Some(folder) = archive
            .stream_map
            .file_folder_index
            .get(fi)
            .copied()
            .flatten()
        else {
            return Ok(()); // 无数据流（0 字节）条目：无内容可验
        };
        let pw_bytes = pw.to_vec();
        let mut src = std::fs::File::open(&self.path).map_err(VfsError::Io)?;
        let dec = sevenz_rust::BlockDecoder::new(folder, archive, &pw_bytes, &mut src);
        let mut hit = false;
        dec.for_each_entries(&mut |e, data| {
            let is_target = e.name == entry.name && !hit;
            if is_target {
                hit = true;
            }
            std::io::copy(data, &mut std::io::sink()).map_err(sevenz_rust::Error::io)?;
            Ok(!is_target) // 目标已验：停止迭代
        })
        .map_err(map_7z_err)?;
        Ok(())
    }
}

/// copy_with_progress 的错误：区分读侧/写侧——7z 读侧错误（解码/CRC 失败）
/// 可能是密码错误，需保留 io 形态让 sevenz-rust 转成 MaybeBadPassword
pub(crate) enum CopyErr {
    Read(std::io::Error),
    Write(std::io::Error),
}

/// 拷贝并按 4MB 粒度回调进度
pub(crate) fn copy_with_progress(
    src: &mut dyn Read,
    dest: &mut impl Write,
    total: u64,
    progress: &dyn Fn(u64),
) -> Result<u64, CopyErr> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done: u64 = 0;
    let mut last_report: u64 = 0;
    progress(0);
    loop {
        let n = src.read(&mut buf).map_err(CopyErr::Read)?;
        if n == 0 {
            break;
        }
        dest.write_all(&buf[..n]).map_err(CopyErr::Write)?;
        done += n as u64;
        if done - last_report >= 4 * 1024 * 1024 || done >= total {
            progress(done);
            last_report = done;
        }
    }
    Ok(done)
}

/// 计数丢弃 sink：solid 流内非目标条目的数据必须顺序消费（不可跳），
/// 但内容无用——就地统计字节数并按 4MB 粒度上报所属段（SkipUp/
/// Downstream）的进度。base = 段内已累计游标（跨 folder 连续），total =
/// 段定比总量（0 = 无分母，UI 走活动指示）
struct PhaseSink<'a> {
    phase: ExtractPhase,
    base: u64,
    total: u64,
    n: u64,
    last_report: u64,
    progress: &'a dyn Fn(ExtractPhase, u64, u64),
}

impl<'a> PhaseSink<'a> {
    fn new(
        phase: ExtractPhase,
        base: u64,
        total: u64,
        progress: &'a dyn Fn(ExtractPhase, u64, u64),
    ) -> Self {
        Self {
            phase,
            base,
            total,
            n: 0,
            last_report: 0,
            progress,
        }
    }
}

impl Write for PhaseSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.n += buf.len() as u64;
        if self.n - self.last_report >= 4 * 1024 * 1024 {
            let cur = if self.total > 0 {
                (self.base + self.n).min(self.total)
            } else {
                self.base + self.n
            };
            (self.progress)(self.phase, cur, self.total);
            self.last_report = self.n;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 单条目落地：临时文件 + 进度拷贝 + flush + 原子改名，失败清理半截临时文件
/// （并发解压同一 dest 时互不覆盖写坏缓存）。读侧错误（解码/CRC 失败）保留
/// io 形态——可能是坏密码，sevenz-rust 在 maybe_bad_password 里把 io 错误
/// 转成 MaybeBadPassword → VfsError::Password（UI 弹密码框重试的关键路径）。
/// 进度按 (phase, base+已写字节, total) 上报：total=0 无分母走活动指示，
/// base = 该条目在本段内的起始游标（Target 段传 0/条目自身尺寸）
fn write_entry_data(
    data: &mut dyn Read,
    size: u64,
    dest: &Path,
    phase: ExtractPhase,
    base: u64,
    total: u64,
    progress: &dyn Fn(ExtractPhase, u64, u64),
) -> Result<(), sevenz_rust::Error> {
    let tmp = super::tmp_path(dest);
    let res = (|| -> Result<(), sevenz_rust::Error> {
        let mut out = std::fs::File::create(&tmp).map_err(|err| {
            sevenz_rust::Error::other(format!("创建临时文件失败 [{}]: {err}", tmp.display()))
        })?;
        copy_with_progress(data, &mut out, size, &|done| {
            let cur = if total > 0 {
                (base + done).min(total)
            } else {
                base + done
            };
            progress(phase, cur, total)
        })
        .map_err(|err| match err {
            CopyErr::Read(e) => sevenz_rust::Error::io(e),
            CopyErr::Write(e) => {
                sevenz_rust::Error::other(format!("写入临时文件失败 [{}]: {e}", tmp.display()))
            }
        })?;
        out.flush()
            .map_err(|err| sevenz_rust::Error::other(err.to_string()))?;
        std::fs::rename(&tmp, dest).map_err(|err| {
            sevenz_rust::Error::other(format!(
                "重命名临时文件失败: [{}] → [{}]: {err}",
                tmp.display(),
                dest.display()
            ))
        })?;
        Ok(())
    })();
    if res.is_err() {
        std::fs::remove_file(&tmp).ok(); // 清掉半截临时文件
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 sevenz-rust 自带 writer 纯 Rust 造测试包（无需外部 7z.exe；
    /// 运行时解码同样内置，分发包无需携带任何 7z 程序）
    fn make_test_7z() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("sevenz_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let payload = dir.join("a.bin");
        std::fs::write(&payload, vec![7u8; 100_000]).ok()?;
        let arch = dir.join("test.7z");
        let mut w = sevenz_rust::SevenZWriter::create(&arch).ok()?;
        // from_path 的第二参即 entry 名——直接用带子目录的路径名，验证多级路径列出
        let entry = sevenz_rust::SevenZArchiveEntry::from_path(&payload, "sub/dir/a.bin".into());
        w.push_archive_entry(entry, Some(std::fs::File::open(&payload).ok()?))
            .ok()?;
        w.finish().ok()?;
        std::fs::remove_file(&payload).ok()?;
        Some(arch)
    }

    #[test]
    fn list_and_extract() {
        let Some(path) = make_test_7z() else {
            panic!("构造 7z 测试包失败");
        };
        let vfs = SevenzVFS::open(&path, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        let a = entries[0].clone();
        assert_eq!(a.name, "sub/dir/a.bin");
        assert_eq!(a.size, 100_000);
        assert!(!a.encrypted);

        let dest = path.parent().unwrap().join("out_a.bin");
        let seen = std::sync::atomic::AtomicU64::new(0);
        vfs.extract_entry(
            &a,
            &dest,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
        )
        .unwrap();
        assert!(
            seen.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "进度回调未触发"
        );
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len(), 100_000);
        assert!(data.iter().all(|&b| b == 7));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// solid 压缩回归测试：多个文件压进同一个 folder（顺序解压流），
    /// 解压非首个条目必须成功且内容正确。
    ///
    /// 曾有 bug：extract_entry 回调对非目标 entry 不读数据直接跳过，
    /// 而 solid folder 内文件首尾相接、BoundedReader 不带 drop-skip，
    /// 导致目标 entry 从流中错位位置读取 → ChecksumVerificationFailed。
    /// 56G 图片合集等 solid 大包解压任意非首条目必失败，此测试防回归
    #[test]
    fn solid_7z_extract_later_entry() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};
        use std::io::Read as _;

        let dir = std::env::temp_dir().join(format!("sevenz_solid_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 三个不同内容的源文件（不同字节模式便于验证不错位）
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("f1.bin".into(), vec![1u8; 40_000]),
            ("f2.bin".into(), vec![2u8; 30_000]),
            ("f3.bin".into(), vec![3u8; 20_000]),
        ];
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("solid.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        // push_archive_entries（复数）：所有 entry 压进一个 folder = solid
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let listed = vfs.list();
        assert_eq!(listed.len(), 3, "solid 包应列出全部 3 个条目");

        // 逐个解压并验证内容（重点：第 2、3 个 entry，前面有需跳过的数据）
        for (i, (name, expect)) in srcs.iter().enumerate() {
            let e = &listed[i];
            assert_eq!(e.name, *name);
            let dest = dir.join(format!("out_{name}"));
            vfs.extract_entry(
                e,
                &dest,
                &std::sync::atomic::AtomicBool::new(false),
                &|_, _, _| {},
            )
            .unwrap_or_else(|e| panic!("solid 包第 {} 个条目 [{}] 解压失败: {e}", i + 1, name));
            let mut data = Vec::new();
            std::fs::File::open(&dest)
                .unwrap()
                .read_to_end(&mut data)
                .unwrap();
            assert_eq!(data, *expect, "条目 [{name}] 内容错位（CRC/顺序 bug 复现）");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 多 folder（非 solid，每文件独立 folder）批量解压回归测试：
    /// 旧实现 for_each_entries 从 folder 0 顺序解压全部前置 folder（56G 包里
    /// 目标排第 400 位也要先解压前面几十 GB，10 分钟级）；新实现 BlockDecoder
    /// 直达目标 folder，前置 folder 只 seek 不解压。本测试验证：
    /// 1) 多 folder 包解压后面的条目内容正确；2) 批量跨 folder 一次解出 + file_done 逐个回调
    #[test]
    fn multi_folder_batch_extract_skips_earlier_folders() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};
        use std::io::Read as _;

        let dir = std::env::temp_dir().join(format!("sevenz_mf_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("a.bin".into(), vec![1u8; 30_000]),
            ("b.bin".into(), vec![2u8; 20_000]),
            ("c.bin".into(), vec![3u8; 40_000]),
        ];
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("multi_folder.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        // push_archive_entry（单数）：每个文件独立 folder（非 solid）
        for p in &paths {
            let entry = SevenZArchiveEntry::from_path(
                p,
                p.file_name().unwrap().to_string_lossy().into_owned(),
            );
            w.push_archive_entry(entry, Some(std::fs::File::open(p).unwrap()))
                .unwrap();
        }
        w.finish().unwrap();

        // 前置断言：确实是多 folder 包（否则测不到 folder 跳过路径）
        {
            let r = sevenz_rust::SevenZReader::open(&arch, Password::empty()).unwrap();
            assert!(
                r.archive().folders.len() >= 3,
                "测试包应为多 folder（实际 {} 个），push_archive_entry 行为变化？",
                r.archive().folders.len()
            );
        }

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let listed = vfs.list();
        assert_eq!(listed.len(), 3);

        // 单条解压最后面的条目（旧实现需顺序解压 folder 0/1；新实现直达 folder 2）
        let dest_c = dir.join("out_c.bin");
        vfs.extract_entry(
            &listed[2],
            &dest_c,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        let mut data = Vec::new();
        std::fs::File::open(&dest_c)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        assert_eq!(data, srcs[2].1, "folder 直达解压内容错误");

        // 批量：a + c 一次解出（跨 folder），file_done 按完成顺序回调
        let dest_a = dir.join("batch_a.bin");
        let dest_c2 = dir.join("batch_c.bin");
        let done = std::sync::atomic::AtomicU64::new(0);
        vfs.extract_batch(
            &[(&listed[0], &dest_a), (&listed[2], &dest_c2)],
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
            &|_| {
                done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
        )
        .unwrap();
        assert_eq!(
            done.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "file_done 应恰好回调 2 次"
        );
        assert_eq!(std::fs::read(&dest_a).unwrap(), srcs[0].1);
        assert_eq!(std::fs::read(&dest_c2).unwrap(), srcs[2].1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// solid 包批量解压：一次顺序解压流解出多个条目（预取的场景），
    /// 中间和末尾条目都正确、file_done 逐个回调
    #[test]
    fn solid_7z_batch_extract() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_sb_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("f1.bin".into(), vec![1u8; 40_000]),
            ("f2.bin".into(), vec![2u8; 30_000]),
            ("f3.bin".into(), vec![3u8; 20_000]),
        ];
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("solid_batch.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let listed = vfs.list();
        assert_eq!(listed.len(), 3);

        // 一次解 f1 + f3（f2 在中间被顺序跳过）
        let d1 = dir.join("b1.bin");
        let d3 = dir.join("b3.bin");
        let done = std::sync::atomic::AtomicU64::new(0);
        vfs.extract_batch(
            &[(&listed[0], &d1), (&listed[2], &d3)],
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
            &|_| {
                done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
        )
        .unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(std::fs::read(&d1).unwrap(), srcs[0].1);
        assert_eq!(std::fs::read(&d3).unwrap(), srcs[2].1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// locate_start（上游预取免费区）：solid 单 folder 包 → 前缀整段免费；
    /// 多 folder 包 → 免费区从目标所在 folder 首条目起，不跨 folder
    /// （上游预取不得把前置 folder 的条目当免费数据）
    #[test]
    fn locate_start_zone_boundaries() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_ls_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("f1.bin".into(), vec![1u8; 30_000]),
            ("f2.bin".into(), vec![2u8; 20_000]),
            ("f3.bin".into(), vec![3u8; 25_000]),
            ("f4.bin".into(), vec![4u8; 15_000]),
        ];
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        // 两次 push_archive_entries = 两个 solid folder：f1+f2 / f3+f4
        let arch = dir.join("two_folders.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        for group in [&paths[..2], &paths[2..]] {
            let entries: Vec<SevenZArchiveEntry> = group
                .iter()
                .map(|p| {
                    SevenZArchiveEntry::from_path(
                        p,
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                    )
                })
                .collect();
            let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = group
                .iter()
                .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
                .collect();
            w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
                .unwrap();
        }
        w.finish().unwrap();

        // 前置断言：确实是双 folder 包（否则测不到边界）
        {
            let r = sevenz_rust::SevenZReader::open(&arch, Password::empty()).unwrap();
            assert_eq!(
                r.archive().folders.len(),
                2,
                "两次 push_archive_entries 应产生两个 folder，writer 行为变化？"
            );
        }

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        assert_eq!(vfs.list().len(), 4);
        // folder 0 内：f2 的免费区从 f1 起（整个 folder 0 前缀）
        assert_eq!(vfs.locate_start(1), Some(0));
        // folder 1 内：f4 的免费区从 f3 起，不得把 f1/f2（前置 folder）划进来
        assert_eq!(
            vfs.locate_start(3),
            Some(2),
            "上游预取免费区不得超出目标所在 folder"
        );
        // 各 folder 首条目：免费区为空（起点即自身）
        assert_eq!(vfs.locate_start(0), Some(0));
        assert_eq!(vfs.locate_start(2), Some(2));
        // 越界/无数据流条目：None（不 panic）
        assert_eq!(vfs.locate_start(4), None);
        // 双 folder（每 folder 2 条目）= solid：路由 Solid
        assert_eq!(vfs.access_mode(), AccessMode::Solid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 7z solid/非 solid 路由：全独立 folder（-ms=off，每文件一个 folder）
    /// → AccessMode::SingleFile + 无上游免费区（folder 直达是廉价 seek），
    /// 中间条目直接解出 = 真随机访问；solid 单 folder → Solid + 整前缀
    /// 免费区（locate_start = Some(0)）
    #[test]
    fn sevenz_solid_routing() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_route_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("f0.bin".into(), vec![7u8; 30_000]),
            ("f1.bin".into(), vec![8u8; 30_000]),
            ("f2.bin".into(), vec![9u8; 30_000]),
        ];
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }

        // ---- 非 solid：每文件独立 folder（push_archive_entry 单数）----
        let ns_arch = dir.join("nonsolid.7z");
        let mut w = SevenZWriter::create(&ns_arch).unwrap();
        for p in &paths {
            let entry = SevenZArchiveEntry::from_path(
                p,
                p.file_name().unwrap().to_string_lossy().into_owned(),
            );
            w.push_archive_entry(entry, Some(std::fs::File::open(p).unwrap()))
                .unwrap();
        }
        w.finish().unwrap();
        let ns = SevenzVFS::open(&ns_arch, None).unwrap();
        assert_eq!(
            ns.access_mode(),
            AccessMode::SingleFile,
            "全独立 folder 应走单文件按需"
        );
        for i in 0..3 {
            assert_eq!(
                ns.locate_start(i),
                None,
                "非 solid 7z folder 直达无免费区，不应返回 Some"
            );
        }
        // 中间条目直接解出（真随机访问，不依赖前缀顺序流）
        let listed = ns.list();
        assert_eq!(listed.len(), 3);
        let dest = dir.join("ns_mid.bin");
        ns.extract_entry(
            &listed[1],
            &dest,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), srcs[1].1);

        // ---- solid：全部条目压进同一 folder（push_archive_entries 复数）----
        let so_arch = dir.join("solid.7z");
        let mut w = SevenZWriter::create(&so_arch).unwrap();
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();
        let so = SevenzVFS::open(&so_arch, None).unwrap();
        assert_eq!(
            so.access_mode(),
            AccessMode::Solid,
            "单 folder 顺序流应走滑窗会话"
        );
        assert_eq!(
            so.locate_start(2),
            Some(0),
            "solid 包目标前整前缀都是免费区"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AES256 内容加密的 7z（头不加密）：list 可见且 encrypted=true；
    /// 无密码/错密码解压报 Password，正确密码解压成功
    #[test]
    fn encrypted_7z_password_flow() {
        let dir = std::env::temp_dir().join(format!("sevenz_enc_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let payload = dir.join("p.bin");
        std::fs::write(&payload, vec![5u8; 60_000]).unwrap();
        let arch = dir.join("enc.7z");
        let mut w = sevenz_rust::SevenZWriter::create(&arch).unwrap();
        // 头不加密（只加密内容）：list 无密码可见；加密头场景 open 阶段就要密码
        w.set_encrypt_header(false);
        w.set_content_methods(vec![sevenz_rust::AesEncoderOptions::new(Password::from(
            "pw123",
        ))
        .into()]);
        let entry = sevenz_rust::SevenZArchiveEntry::from_path(&payload, "enc/p.bin".into());
        w.push_archive_entry(entry, Some(std::fs::File::open(&payload).unwrap()))
            .unwrap();
        w.finish().unwrap();
        std::fs::remove_file(&payload).unwrap();

        // 1) 无密码打开（头未加密）：list 可见（sevenz-rust 不暴露条目加密标志，encrypted=false）
        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        // 2) 无密码解压 → Password
        let dest = dir.join("out.bin");
        // 2) 无密码解压 → Password
        assert!(matches!(
            vfs.extract_entry(
                &entries[0],
                &dest,
                &std::sync::atomic::AtomicBool::new(false),
                &|_, _, _| {}
            ),
            Err(VfsError::Password)
        ));
        // 3) 错密码解压 → Password
        let vfs_bad = SevenzVFS::open(&arch, Some("wrong")).unwrap();
        assert!(matches!(
            vfs_bad.extract_entry(
                &entries[0],
                &dest,
                &std::sync::atomic::AtomicBool::new(false),
                &|_, _, _| {}
            ),
            Err(VfsError::Password)
        ));
        // 4) 正确密码解压成功
        let vfs_ok = SevenzVFS::open(&arch, Some("pw123")).unwrap();
        vfs_ok
            .extract_entry(
                &entries[0],
                &dest,
                &std::sync::atomic::AtomicBool::new(false),
                &|_, _, _| {},
            )
            .unwrap();
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len(), 60_000);
        assert!(data.iter().all(|&b| b == 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- extract_session（持久解码会话）协议测试 ----

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

    /// 驻留语义测试驱动：真暂停后线程不因"targets 解空"退出，同步调用
    /// extract_session 会永久阻塞。此 helper 把会话放到后台线程跑，批解净
    /// （targets 清空 = 全部 rename 落地）后置 cancel 让线程退出，返回线程
    /// 最终 Result——cancel 路径为 Err(Cancelled)（同时 alive 被收尾置 false）。
    /// 退出路径断言（alive=false）在新语义下由 cancel 收尾保证，语义等价
    fn run_session_until_drained(
        s: &SharedSession,
        f: impl FnOnce(&SharedSession, &std::sync::atomic::AtomicBool) -> Result<(), VfsError>
        + Send
        + 'static,
    ) -> Result<(), VfsError> {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c2 = cancel.clone();
        let s2 = s.clone();
        let handle = std::thread::spawn(move || f(&s2, &c2));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let empty = s.lock().unwrap().targets.is_empty();
            if empty || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap()
    }

    /// 建多 folder 测试包（每文件独立 folder，push_archive_entry）
    fn make_multi_folder_7z(dir: &Path, srcs: &[(String, Vec<u8>)]) -> PathBuf {
        let mut paths = Vec::new();
        for (name, data) in srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("multi.7z");
        let mut w = sevenz_rust::SevenZWriter::create(&arch).unwrap();
        for p in &paths {
            let entry = sevenz_rust::SevenZArchiveEntry::from_path(
                p,
                p.file_name().unwrap().to_string_lossy().into_owned(),
            );
            w.push_archive_entry(entry, Some(std::fs::File::open(p).unwrap()))
                .unwrap();
        }
        w.finish().unwrap();
        arch
    }

    /// focus 陈旧指向已落地条目（targets 已 remove、UI 未重定向，会话继续
    /// 解下游预取）时分段总量必须与解码回调同源：非目标条目（含陈旧 focus）
    /// 按回调归 Downstream，不得发 Target 段骨架——旧实现按 i == focus_idx
    /// 先判 Target，Target bar 卡 0% 且 Downstream 串量。
    /// 场景：solid 包 3 条目，f1 预先落好（缓存命中后已 remove），targets
    /// 只剩 f2/f3，focus 残留指向 f1 的下标 0
    #[test]
    fn solid_7z_session_stale_focus_no_target_skeleton() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_stale_focus_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("f1.bin".into(), vec![1u8; 40_000]),
            ("f2.bin".into(), vec![2u8; 30_000]),
            ("f3.bin".into(), vec![3u8; 20_000]),
        ];
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("solid3.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(p, p.file_name().unwrap().to_string_lossy().into_owned())
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();

        let vfs = SevenzVFS::open(&arch, None).unwrap();

        // f1 已落地（缓存命中后的常态）：内容正确
        let d1 = dir.join("out_f1.bin");
        vfs.extract_entry(
            &vfs.list()[0],
            &d1,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(&d1).unwrap(), srcs[0].1);

        // targets 只剩 f2/f3（f1 已 remove），focus 残留指向 f1（idx 0）
        let s = new_session();
        let d2 = dir.join("out_f2.bin");
        let d3 = dir.join("out_f3.bin");
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f2.bin".into(), (1, d2.clone()));
            st.targets.insert("f3.bin".into(), (2, d3.clone()));
            st.focus = Some(0);
            st.alive = true;
        }
        let target_reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let down_total = std::sync::Arc::new(std::sync::Mutex::new(0u64));
        let tr = target_reports.clone();
        let dt = down_total.clone();
        run_session_until_drained(&s, move |s, cancel| {
            vfs.extract_session(
                s,
                cancel,
                &|ph, cur, total| {
                    if ph == ExtractPhase::Target {
                        tr.lock().unwrap().push((cur, total));
                    }
                    if ph == ExtractPhase::Downstream {
                        *dt.lock().unwrap() = total;
                    }
                },
                &|_, _| {},
            )
        })
        .err()
        .expect("驻留会话由 cancel 收尾");

        // 旧实现：f1（idx 0 == focus 且不在 targets）被算进 Target 段 → 发
        // Target(0, 40_000) 骨架但回调走非目标分支永不写它 → 卡 0%。
        // 新实现：Target 段总量 0，全程不出现非零 Target 上报
        assert!(
            target_reports.lock().unwrap().iter().all(|&(_, t)| t == 0),
            "陈旧 focus 不在 targets 时不得发非零 Target 段: {:?}",
            target_reports.lock().unwrap()
        );
        // f1 与回调同源归入 Downstream：总量 = 三条目之和（旧实现只有 50_000）
        assert_eq!(
            *down_total.lock().unwrap(),
            90_000,
            "Downstream 总量应含陈旧 focus 条目"
        );
        assert_eq!(std::fs::read(&d2).unwrap(), srcs[1].1, "f2 内容错误");
        assert_eq!(std::fs::read(&d3).unwrap(), srcs[2].1, "f3 内容错误");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话生命周期端到端（驻留语义）：
    /// 1) 目标解净 → 批完成驻留（targets 清空、线程等待），由 cancel 收尾
    /// 2) 空目标再入 = 驻留等待直至 cancel 收尾（Err(Cancelled)），不触碰任何文件
    /// 3) 会话复用：UI 重新加目标 + 置 alive，同一会话对象再跑；
    ///    focus folder 抢先解（file_done 顺序验证）
    /// 4) 目标不在包内：无进展保险报错防死循环（alive=false）
    #[test]
    fn session_lifecycle_and_reuse() {
        let dir = std::env::temp_dir().join(format!("sevenz_sess_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("f1.bin".into(), vec![1u8; 30_000]),
            ("f2.bin".into(), vec![2u8; 20_000]),
            ("f3.bin".into(), vec![3u8; 25_000]),
            ("f4.bin".into(), vec![4u8; 15_000]),
        ];
        let arch = make_multi_folder_7z(&dir, &srcs);
        // Arc 包装：多个子步骤的后台会话闭包都要 move 一份
        let vfs = std::sync::Arc::new(SevenzVFS::open(&arch, None).unwrap());
        assert_eq!(vfs.list().len(), 4);

        // 1) 基本解出：目标 f3（folder 直达，前置 folder 只 seek 不解压）
        let s = new_session();
        let d3 = dir.join("s3.bin");
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f3.bin".into(), (2, d3.clone()));
            st.focus = Some(2);
            st.alive = true;
        }
        let done_idx = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        let di = done_idx.clone();
        let d3_expect = d3.clone();
        let vfs1 = vfs.clone();
        run_session_until_drained(&s, move |s, cancel| {
            vfs1.extract_session(
                s,
                cancel,
                &|_, _, _| {},
                &|idx, path| {
                    di.store(idx as u64, std::sync::atomic::Ordering::Relaxed);
                    assert_eq!(path, d3_expect.as_path(), "落地路径应为 f3 的目标路径");
                },
            )
        })
        .err()
        .expect("驻留会话由 cancel 收尾");
        assert_eq!(
            done_idx.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "file_done 应回调 f3 的 list 下标"
        );
        assert_eq!(std::fs::read(&d3).unwrap(), srcs[2].1, "f3 内容错误");
        {
            let st = s.lock().unwrap();
            assert!(st.targets.is_empty(), "解净后 targets 应清空");
            assert!(!st.alive, "Ok 返回后 alive 应为 false");
        }

        // 2) 空目标再入：驻留等待直至 cancel 收尾（不再立即 Ok——空目标
        // 即驻留是新的空闲语义），期间不触碰任何文件
        let vfs2 = vfs.clone();
        let err2 = run_session_until_drained(&s, move |s, cancel| {
            vfs2.extract_session(s, cancel, &|_, _, _| {}, &|_, _| {})
        })
        .err()
        .expect("空目标会话由 cancel 收尾");
        assert!(
            matches!(err2, VfsError::Cancelled),
            "空目标驻留应由 cancel 收尾: {err2:?}"
        );

        // 3) 会话复用 + focus 抢道：目标 f1 + f4，focus=f4 → f4 先落地
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f1.bin".into(), (0, dir.join("s1.bin")));
            st.targets.insert("f4.bin".into(), (3, dir.join("s4.bin")));
            st.focus = Some(3);
            st.alive = true;
        }
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let od = order.clone();
        let vfs3 = vfs.clone();
        run_session_until_drained(&s, move |s, cancel| {
            vfs3.extract_session(s, cancel, &|_, _, _| {}, &|idx, _| {
                od.lock().unwrap().push(idx)
            })
        })
        .err()
        .expect("驻留会话由 cancel 收尾");
        assert_eq!(
            *order.lock().unwrap(),
            vec![3, 0],
            "focus folder 应最先解（f4 → f1）"
        );
        assert_eq!(std::fs::read(dir.join("s1.bin")).unwrap(), srcs[0].1);
        assert_eq!(std::fs::read(dir.join("s4.bin")).unwrap(), srcs[3].1);

        // 4) 目标不在包内：无进展保险报错，防死循环
        let s2 = new_session();
        {
            let mut st = s2.lock().unwrap();
            st.targets
                .insert("no_such.bin".into(), (0, dir.join("nope.bin")));
            st.alive = true;
        }
        assert!(matches!(
            vfs.extract_session(
                &s2,
                &std::sync::atomic::AtomicBool::new(false),
                &|_, _, _| {},
                &|_, _| { panic!("不存在的目标不应落地") }
            ),
            Err(VfsError::Archive(_))
        ));
        assert!(!s2.lock().unwrap().alive, "报错路径应置 alive=false");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gen 救回回归：零落地轮若期间 UI 重定向过（gen 变化），必须重新
    /// 规划续跑而非误报"无进展"错误。
    ///
    /// 复现场景：solid 单 folder 65 条目，初始目标只有末位 n64；解码流
    /// 顺序消费 n00-n63 时（skipped=64 处的 progress 回调模拟 UI 重定向
    /// 协议：targets 换新 + focus 改写 + gen 自增）——本轮剩余流仅剩的
    /// n64 已非目标，零落地。旧代码此时误报 Err 中止整个会话；新代码
    /// 对比 gen 0→1，重新规划从头解出 n00 后干净收尾
    #[test]
    fn session_zero_landing_rescued_by_gen() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_gen_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 65 × 4KB 互异内容（i%251；skipped=64 的 progress 触发点恰在 n64 前）
        let srcs: Vec<(String, Vec<u8>)> = (0..65)
            .map(|i| (format!("n{i:02}.bin"), vec![(i % 251) as u8; 4096]))
            .collect();
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("solid65.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        // push_archive_entries（复数）：全部压进同一 folder = solid
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        assert_eq!(vfs.list().len(), 65);

        let s = new_session();
        let d0 = dir.join("out_n00.bin");
        let d64 = dir.join("out_n64.bin");
        let d0_expect = d0.clone();
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("n64.bin".into(), (64, d64.clone()));
            st.focus = Some(64);
            st.alive = true;
        }
        // progress 回调里模拟 UI 重定向（一次性）：跳过段字节数非零（即
        // n63 消费完、n64 未到——初始 (0, total) 骨架上报被 cur>0 过滤掉）
        // 时整体换目标——与 UI 重定向锁块同协议
        let redirected = std::sync::atomic::AtomicBool::new(false);
        let redirected = std::sync::Arc::new(redirected);
        let red = redirected.clone();
        let s_ref = s.clone();
        run_session_until_drained(&s, move |s, cancel| {
            vfs.extract_session(
                s,
                cancel,
                &|phase, cur, _total| {
                    if phase != ExtractPhase::SkipUp || cur == 0 {
                        return;
                    }
                    if red.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let mut st = s_ref.lock().unwrap();
                    st.targets.clear();
                    st.targets.insert("n00.bin".into(), (0, d0.clone()));
                    st.focus = Some(0);
                    st.gen += 1;
                },
                &|idx, _| {
                    assert_eq!(idx, 0, "重定向后唯一应落地的目标是 n00");
                },
            )
        })
        .err()
        .expect("gen 救回应重新规划续跑且由 cancel 收尾");
        assert_eq!(
            std::fs::read(&d0_expect).unwrap(),
            srcs[0].1,
            "重定向后 n00 应从头解出且内容正确"
        );
        assert!(!d64.exists(), "被重定向丢弃的 n64 不应落地");
        {
            let st = s.lock().unwrap();
            assert!(st.targets.is_empty(), "终态 targets 应为空");
            assert!(!st.alive, "终态会话应干净退出");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 动态目标集：file_done 回调里追加下一个目标，单次会话调用链式解出
    /// 全部条目（每轮规划重读 targets）。这正是 UI 预取期间用户翻页/点击
    /// 追加目标的协议路径（锁外回调 → 回调内可安全再锁）
    #[test]
    fn session_dynamic_targets_chain() {
        let dir = std::env::temp_dir().join(format!("sevenz_chain_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("f1.bin".into(), vec![1u8; 30_000]),
            ("f2.bin".into(), vec![2u8; 20_000]),
            ("f3.bin".into(), vec![3u8; 25_000]),
            ("f4.bin".into(), vec![4u8; 15_000]),
        ];
        let arch = make_multi_folder_7z(&dir, &srcs);
        let vfs = SevenzVFS::open(&arch, None).unwrap();
        assert_eq!(vfs.list().len(), 4);

        let s = new_session();
        let dests: Vec<PathBuf> = (0..4).map(|i| dir.join(format!("chain_{i}.bin"))).collect();
        let dests_in_thread = dests.clone();
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f1.bin".into(), (0, dests[0].clone()));
            st.focus = Some(0);
            st.alive = true;
        }
        let done = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dn = done.clone();
        let s_ref = s.clone();
        run_session_until_drained(&s, move |s, cancel| {
            vfs.extract_session(s, cancel, &|_, _, _| {}, &|idx, _| {
                dn.lock().unwrap().push(idx);
                // 落地即追加下一个目标（模拟 UI 组批）
                if idx + 1 < 4 {
                    let mut st = s_ref.lock().unwrap();
                    st.targets.insert(
                        format!("f{}.bin", idx + 2),
                        (idx + 1, dests_in_thread[idx + 1].clone()),
                    );
                }
            })
        })
        .err()
        .expect("驻留会话由 cancel 收尾");
        assert_eq!(
            *done.lock().unwrap(),
            vec![0, 1, 2, 3],
            "应按链式追加顺序全部落地"
        );
        for (i, (_, data)) in srcs.iter().enumerate() {
            assert_eq!(
                std::fs::read(&dests[i]).unwrap(),
                *data,
                "f{} 内容错误",
                i + 1
            );
        }
        assert!(!s.lock().unwrap().alive);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// focus 漂移中断 + 自愈重解：folder A（solid g1+g2+g3）+ folder B（h1）。
    /// 目标 = g1 + g3 + h1，focus 初始 g1（folder A 优先）。g1 落地回调里把
    /// focus 切到 h1（另一 folder）→ g2 的持锁快照判漂移 → 中断 folder A →
    /// 外层重排：folder B 先解（h1 落地），再从头重解 folder A 补 g3。
    /// file_done 顺序 = [g1, h1, g3]，中断重解后内容仍正确
    #[test]
    fn session_focus_drift_interrupts_folder() {
        let dir = std::env::temp_dir().join(format!("sevenz_drift_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = vec![
            ("g1.bin".into(), vec![1u8; 20_000]),
            ("g2.bin".into(), vec![2u8; 15_000]),
            ("g3.bin".into(), vec![3u8; 25_000]),
            ("h1.bin".into(), vec![8u8; 18_000]),
        ];
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("drift.7z");
        let mut w = sevenz_rust::SevenZWriter::create(&arch).unwrap();
        // folder A：g1+g2+g3 同 folder（solid）；folder B：h1 独立
        {
            let group = &paths[..3];
            let entries: Vec<sevenz_rust::SevenZArchiveEntry> = group
                .iter()
                .map(|p| {
                    sevenz_rust::SevenZArchiveEntry::from_path(
                        p,
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                    )
                })
                .collect();
            let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = group
                .iter()
                .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
                .collect();
            w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
                .unwrap();
        }
        {
            let p = &paths[3];
            let entry = sevenz_rust::SevenZArchiveEntry::from_path(
                p,
                p.file_name().unwrap().to_string_lossy().into_owned(),
            );
            w.push_archive_entry(entry, Some(std::fs::File::open(p).unwrap()))
                .unwrap();
        }
        w.finish().unwrap();
        // 前置断言：folder A（solid 三条目）+ folder B
        {
            let r = sevenz_rust::SevenZReader::open(&arch, Password::empty()).unwrap();
            assert_eq!(r.archive().folders.len(), 2, "应恰好两个 folder");
        }

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let listed = vfs.list();
        assert_eq!(listed.len(), 4);
        let (g1, g3, h1) = (0usize, 2usize, 3usize);

        let s = new_session();
        let d = |n: &str| dir.join(n);
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("g1.bin".into(), (g1, d("o_g1.bin")));
            st.targets.insert("g3.bin".into(), (g3, d("o_g3.bin")));
            st.targets.insert("h1.bin".into(), (h1, d("o_h1.bin")));
            st.focus = Some(g1);
            st.alive = true;
        }
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let od = order.clone();
        let s_ref = s.clone();
        run_session_until_drained(&s, move |s, cancel| {
            vfs.extract_session(s, cancel, &|_, _, _| {}, &|idx, _| {
                od.lock().unwrap().push(idx);
                if idx == g1 {
                    // g1 刚落地：用户跳到另一 folder 的 h1 → 漂移
                    s_ref.lock().unwrap().focus = Some(h1);
                }
            })
        })
        .err()
        .expect("驻留会话由 cancel 收尾");
        assert_eq!(
            *order.lock().unwrap(),
            vec![g1, h1, g3],
            "漂移后应中断 folder A，先解 focus 所在 folder B，再回头补 g3"
        );
        assert_eq!(std::fs::read(d("o_g1.bin")).unwrap(), srcs[0].1);
        assert_eq!(
            std::fs::read(d("o_g3.bin")).unwrap(),
            srcs[2].1,
            "中断重解后 g3 内容错误（自愈失败）"
        );
        assert_eq!(std::fs::read(d("o_h1.bin")).unwrap(), srcs[3].1);
        assert!(!s.lock().unwrap().alive);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 唤醒改判回归（自死锁防线）：非目标分支批边界驻留（唤醒点条目数据
    /// 未消费）被新批收编后必须当场改判落地。2021 edition 下若把
    /// session.lock() 直接写进 if let scrutinee，守卫临时值存活到整个
    /// if-let 块结束，块内 targets.remove 重入加锁 = 同线程 Mutex 自死锁
    /// （2026-11-28 实测：改判条目文件已完整落地但 file_done 永不触发、
    /// 解码线程冻结、无 panic 无自愈）。修复 = 克隆提为 let 语句再 if let。
    /// 场景全确定性：
    /// 1) 首批仅 n0，其 file_done 回调内清空 targets（模拟 UI 换批瞬时空窗）
    /// 2) 解码推进到 n1：非目标 + 本 folder 无目标 → 非目标分支批边界驻留
    ///    （停驻在 n1，数据未消费）
    /// 3) 主线程写入新批 {n1, n2}（收编唤醒点 n1，n2 在其身前保证唤醒
    ///    判定返回 Ok(true) 顺流续解）
    /// 4) n1 改判落地 → file_done(1) 必须到达（旧代码死锁点：write 完成
    ///    后 targets.remove 重入加锁）
    #[test]
    fn session_wake_rejudge_lands_collected_entry() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_wake_rejudge_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("n{i}.bin"), vec![i as u8; 40_000]))
            .collect();
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("solid5.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let s = new_session();
        let landed: std::sync::Arc<std::sync::Mutex<Vec<usize>>> = Default::default();
        {
            let mut st = s.lock().unwrap();
            st.targets
                .insert("n0.bin".into(), (0, dir.join("out_n0.bin")));
            st.focus = Some(0);
            st.alive = true;
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, cancel2, landed2) = (s.clone(), cancel.clone(), landed.clone());
        let handle = std::thread::spawn(move || {
            vfs.extract_session(&s2, &cancel2, &|_, _, _| {}, &|idx, _path| {
                landed2.lock().unwrap().push(idx);
                if idx == 0 {
                    // n0 落地即清空 targets（锁已释放，模拟 UI 换批瞬时空窗）：
                    // 下一条目 n1 成为非目标且本 folder 无目标 → 非目标分支驻留
                    s2.lock().unwrap().targets.clear();
                }
            })
        });

        // 等 n0 落地，再留出余量确保解码线程进入 n1 的驻留等待（pause 以
        // 50ms 分片轮询，此间不持锁）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !landed.lock().unwrap().contains(&0) {
            assert!(std::time::Instant::now() < deadline, "n0 落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 新批收编唤醒点 n1（焦点）+ n2（n1 身前目标，保证唤醒判定
        // ahead_in_folder=true → Ok(true) 顺流续解而非中断重排）
        let (d1, d2) = (dir.join("out_n1.bin"), dir.join("out_n2.bin"));
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("n1.bin".into(), (1, d1.clone()));
            st.targets.insert("n2.bin".into(), (2, d2.clone()));
        }

        // 改判落地判据 = file_done(1)：旧代码在此前 targets.remove 重入
        // 加锁自死锁（文件已落地但回调永不触发），10s 未达即判复发
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !landed.lock().unwrap().contains(&1) {
            assert!(
                std::time::Instant::now() < deadline,
                "唤醒改判 file_done 未到达：if-let scrutinee 自死锁复发（守卫临时值存活整块）"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 顺流续解：n2 随后落地
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !landed.lock().unwrap().contains(&2) {
            assert!(std::time::Instant::now() < deadline, "n2 顺流落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(*landed.lock().unwrap(), vec![0, 1, 2], "落地顺序错误");
        assert_eq!(
            std::fs::read(&d1).unwrap(),
            srcs[1].1,
            "改判落地内容错误"
        );
        assert_eq!(std::fs::read(&d2).unwrap(), srcs[2].1, "n2 内容错误");

        // 收尾：n2 落地后 folder 清 → 批边界驻留等新目标，cancel 退出
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = handle.join().unwrap();
        assert!(
            matches!(r, Err(VfsError::Cancelled)),
            "驻留会话应由 cancel 收尾: {r:?}"
        );
        assert!(!s.lock().unwrap().alive);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 唤醒臂段骨架回归：批边界驻留被新批唤醒、顺流续解时必须按新批
    /// focus/targets 重算段骨架（wake_skeleton）。旧实现把段总量清零走
    /// (x,0) 活动指示 → 唤醒后续解全程无进度条分母（转圈黑箱，上游落盘/
    /// 下游预取都看不见进度）。判据 = 唤醒后 Downstream 段出现 total==单
    /// 条目大小的上报（骨架 (0,total)，旧实现全程 total==0）
    #[test]
    fn session_wake_skeleton_reports_new_totals() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_wake_skel_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("n{i}.bin"), vec![i as u8; 40_000]))
            .collect();
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("solid5.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let s = new_session();
        let landed: std::sync::Arc<std::sync::Mutex<Vec<usize>>> = Default::default();
        {
            let mut st = s.lock().unwrap();
            st.targets
                .insert("n0.bin".into(), (0, dir.join("out_n0.bin")));
            st.focus = Some(0);
            st.alive = true;
        }
        let reports: std::sync::Arc<
            std::sync::Mutex<Vec<(crate::vfs::ExtractPhase, u64, u64)>>,
        > = Default::default();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, cancel2, landed2, rep, s3) = (
            s.clone(),
            cancel.clone(),
            landed.clone(),
            reports.clone(),
            s.clone(), // file_done 闭包专用克隆（&s2 实参借用冲突）
        );
        let handle = std::thread::spawn(move || {
            vfs.extract_session(
                &s2,
                &cancel2,
                &move |ph, cur, total| {
                    rep.lock().unwrap().push((ph, cur, total));
                },
                &move |idx, _path| {
                    landed2.lock().unwrap().push(idx);
                    if idx == 0 {
                        // n0 落地即清空 targets：n1 非目标且 folder 清 → 批边界驻留
                        s3.lock().unwrap().targets.clear();
                    }
                },
            )
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !landed.lock().unwrap().contains(&0) {
            assert!(std::time::Instant::now() < deadline, "n0 落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        // 规划开工置 false（冷启动/重排 = folder 直达重解）
        assert!(!s.lock().unwrap().resume, "规划开工后 resume 应为 false");

        // 模拟 UI 重定向：新批 n1（锚点，focus 推进）+ n2（下游）。写入后
        // 唤醒判定 ahead_in_folder=true → 顺流续解，唤醒臂应发新骨架
        {
            let mut st = s.lock().unwrap();
            st.targets
                .insert("n1.bin".into(), (1, dir.join("out_n1.bin")));
            st.targets
                .insert("n2.bin".into(), (2, dir.join("out_n2.bin")));
            st.focus = Some(1);
        }

        // 判据：Downstream 段出现 total==40_000（骨架 (0,40_000) 或落地报
        // 满）；旧实现唤醒清零 → 全程 total==0，10s 未达即判回归
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let hit = reports
                .lock()
                .unwrap()
                .iter()
                .any(|&(ph, _, t)| ph == crate::vfs::ExtractPhase::Downstream && t == 40_000);
            if hit {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "唤醒后未见 Downstream 非零骨架：段总量重算回归（旧实现清零）"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 唤醒续解置 true（UI 标题切"续解中"的依据）
        assert!(s.lock().unwrap().resume, "唤醒续解后 resume 应为 true");
        // 骨架纯上报口径，不影响推进：n1（改判落地）与 n2 顺流照常
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !landed.lock().unwrap().contains(&2) {
            assert!(std::time::Instant::now() < deadline, "n2 顺流落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(*landed.lock().unwrap(), vec![0, 1, 2], "落地顺序错误");

        // 收尾：folder 解净 → 批边界驻留，cancel 退出
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = handle.join().unwrap();
        assert!(matches!(r, Err(VfsError::Cancelled)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 跳过提示（skip_note）生命周期回归：非目标条目消费边界必须刷新注记
    /// （定位转圈期 UI 显示"正在跳过 <名>（<大小>）"），批边界驻留必须清空。
    /// 观察点取 Target 段写盘中的 progress 上报——此刻前一个非目标（n1）的
    /// 注记必然还在（清空点在写完 rename 后的 targets.remove），无竞态
    #[test]
    fn session_skip_note_lifecycle() {
        use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

        let dir = std::env::temp_dir().join(format!("sevenz_skip_note_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srcs: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("n{i}.bin"), vec![i as u8; 40_000]))
            .collect();
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let arch = dir.join("solid5.7z");
        let mut w = SevenZWriter::create(&arch).unwrap();
        let entries: Vec<SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();

        let vfs = SevenzVFS::open(&arch, None).unwrap();
        let s = new_session();
        {
            let mut st = s.lock().unwrap();
            st.targets
                .insert("n2.bin".into(), (2, dir.join("out_n2.bin")));
            st.focus = Some(2);
            st.alive = true;
        }
        // Target 段写盘期间 skip_note 快照（n1 刚被跳过、n2 尚未落地清空）
        let observed: std::sync::Arc<std::sync::Mutex<Vec<Option<(usize, String, u64)>>>> =
            Default::default();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, ob, cancel2) = (s.clone(), observed.clone(), cancel.clone());
        let s3 = s.clone(); // 传给会话线程的独立克隆（主线程 s 留作断言）
        let handle = std::thread::spawn(move || {
            vfs.extract_session(
                &s3,
                &cancel2,
                &move |phase, _cur, total| {
                    if phase == crate::vfs::ExtractPhase::Target && total == 40_000 {
                        ob.lock().unwrap().push(s2.lock().unwrap().skip_note.clone());
                    }
                },
                &|_, _| {},
            )
        });

        // n2 落地判据：out 文件在场
        let out = dir.join("out_n2.bin");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::fs::metadata(&out).is_err() {
            assert!(std::time::Instant::now() < deadline, "n2 落地超时");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let ob_snap = observed.lock().unwrap().clone();
        assert!(
            ob_snap.iter().any(|n| n
                .as_ref()
                .is_some_and(|(i, name, size)| *i == 1 && name == "n1.bin" && *size == 40_000)),
            "Target 写盘期间应有 n1 的跳过注记: {ob_snap:?}"
        );
        // n2 落地后 folder 清 → 批边界驻留，注记必须清空（驻留期无消费动作）
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            s.lock().unwrap().skip_note.is_none(),
            "驻留期跳过注记应清空"
        );

        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = handle.join().unwrap();
        assert!(
            matches!(r, Err(VfsError::Cancelled)),
            "驻留会话应由 cancel 收尾: {r:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
