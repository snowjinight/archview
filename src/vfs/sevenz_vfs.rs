//! SevenzVFS：7z 格式（solid 走慢路径会话，全独立 folder 走单文件按需）。
//! sevenz-rust 只支持顺序解压（固态块内 seek 需从块头重解），因此 open_entry 不支持，
//! 播放走后台解压到缓存目录 → 播放本地文件。

use super::{AccessMode, ArchiveVFS, SharedSession, VfsError, VirtualEntry, VirtualFile};
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
        let folder_of: Vec<Option<usize>> = archive
            .stream_map
            .file_folder_index
            .iter()
            .copied()
            .collect();
        let mut folder_first = std::collections::HashMap::new();
        let mut folder_size: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
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
            password: if password.is_some() {
                Some(Password::from(password.unwrap()))
            } else {
                None
            },
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
        progress: &dyn Fn(u64, &str),
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
        progress: &dyn Fn(u64, &str),
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
                    write_entry_data(data, e.size, dest, progress)?;
                    remaining.remove(e.name.as_str());
                    file_done(ti);
                    // 本 folder 目标全部解出：停止迭代（后续 folder 由外层循环处理）
                    Ok(left_in_folder > 0)
                } else {
                    // folder 内非目标条目仍需顺序读完丢弃：solid 块内数据首尾
                    // 相接，crate 的 BoundedReader 不带 drop-skip，不消费数据
                    // 会导致后续 entry 读取整体错位、CRC 校验失败
                    // （solid_7z_extract_later_entry 回归测试覆盖此路径）。
                    // 按条目数 + 字节数上报进度（字节可见"在动"，不像卡死）
                    // 读侧错误保留 io 形态（坏密码路径同上）
                    std::io::copy(data, &mut std::io::sink()).map_err(sevenz_rust::Error::io)?;
                    skipped += 1;
                    skipped_bytes += e.size;
                    if skipped % 64 == 0 {
                        progress(skipped_bytes, &format!("跳过前置文件 {skipped}"));
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
    ///   本 folder 目标解净即停（folder 内后续条目不解压）；非目标条目
    ///   顺序消费丢弃（solid 流不可跳，错位必 CRC 失败）
    /// - 漂移中断：focus 移到其它 folder 且仍待解 → Ok(false) 中断当前
    ///   folder 回外层重排（focus folder 最先）——被中断 folder 之后从头
    ///   重解（自愈，成本与"取消重启"旧路径持平）
    /// - 同 folder 回跳上游（目标在本趟已越过位置）：本趟解到尾不会命中，
    ///   下轮规划重排该 folder 从头重解补齐（自愈）
    fn extract_session(
        &self,
        session: &SharedSession,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(u64, &str),
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

        loop {
            // ---- 规划：持锁快照目标集 → folder 分组 + focus 优先 ----
            let (folders, focus_folder, zero_streams, round_gen) = {
                // 判空/取消与置 alive=false 同一次持锁（协议：与 UI 加目标互斥）
                let mut st = session.lock().unwrap();
                if cancel.load(Ordering::Relaxed) {
                    st.alive = false;
                    return Err(VfsError::Cancelled);
                }
                if st.targets.is_empty() {
                    st.alive = false;
                    return Ok(()); // 空闲收尾：UI 之后加目标会重新 spawn
                }
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
                (folders, focus_folder, zero, round_gen)
            };
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
                let mut skipped: usize = 0;
                let mut skipped_bytes: u64 = 0;
                let r = dec.for_each_entries(&mut |e, data| {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(sevenz_rust::Error::other("cancelled"));
                    }
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
                        interrupted = true;
                        return Ok(false); // 中断本 folder，外层重排抢道 focus
                    }
                    if let Some((idx, dest)) = tgt {
                        write_entry_data(data, e.size, &dest, progress)?;
                        let hit = session
                            .lock()
                            .unwrap()
                            .targets
                            .remove(e.name.as_str())
                            .is_some();
                        if hit {
                            file_done(idx, &dest); // 锁已释放
                            landed += 1;
                        }
                        // 本 folder 目标解净：停止迭代（folder 内后续不解压）
                        Ok(!folder_clear)
                    } else {
                        if folder_clear {
                            return Ok(false);
                        }
                        // 非目标条目顺序消费丢弃（solid 流不可跳，同 extract_batch）
                        std::io::copy(data, &mut std::io::sink())
                            .map_err(sevenz_rust::Error::io)?;
                        skipped += 1;
                        skipped_bytes += e.size;
                        if skipped % 64 == 0 {
                            progress(skipped_bytes, &format!("跳过前置文件 {skipped}"));
                        }
                        Ok(true)
                    }
                });
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

/// 单条目落地：临时文件 + 进度拷贝 + flush + 原子改名，失败清理半截临时文件
/// （并发解压同一 dest 时互不覆盖写坏缓存）。读侧错误（解码/CRC 失败）保留
/// io 形态——可能是坏密码，sevenz-rust 在 maybe_bad_password 里把 io 错误
/// 转成 MaybeBadPassword → VfsError::Password（UI 弹密码框重试的关键路径）。
/// extract_batch 与 extract_session 共用
fn write_entry_data(
    data: &mut dyn Read,
    size: u64,
    dest: &Path,
    progress: &dyn Fn(u64, &str),
) -> Result<(), sevenz_rust::Error> {
    let tmp = super::tmp_path(dest);
    let res = (|| -> Result<(), sevenz_rust::Error> {
        let mut out = std::fs::File::create(&tmp).map_err(|err| {
            sevenz_rust::Error::other(format!("创建临时文件失败 [{}]: {err}", tmp.display()))
        })?;
        copy_with_progress(data, &mut out, size, &|done| progress(done, "")).map_err(|err| {
            match err {
                CopyErr::Read(e) => sevenz_rust::Error::io(e),
                CopyErr::Write(e) => {
                    sevenz_rust::Error::other(format!("写入临时文件失败 [{}]: {e}", tmp.display()))
                }
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
            &|_, _| {
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
                &|_, _| {},
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
            &|_, _| {},
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
            &|_, _| {},
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
            &|_, _| {},
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
            &|_, _| {},
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
        assert_eq!(so.access_mode(), AccessMode::Solid, "单 folder 顺序流应走滑窗会话");
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
                &|_, _| {}
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
                &|_, _| {}
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
                &|_, _| {},
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
        }))
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

    /// 会话生命周期端到端：
    /// 1) 目标解净 → Ok、targets 清空、alive=false、file_done 带 list 下标
    /// 2) 空目标再入 = 空闲退出：立即 Ok，不触碰任何文件
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
        let vfs = SevenzVFS::open(&arch, None).unwrap();
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
        let done_idx = std::sync::atomic::AtomicU64::new(u64::MAX);
        vfs.extract_session(
            &s,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _| {},
            &|idx, path| {
                done_idx.store(idx as u64, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(path, d3.as_path(), "落地路径应为 f3 的目标路径");
            },
        )
        .unwrap();
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

        // 2) 空目标 = 空闲退出：立即 Ok
        vfs.extract_session(
            &s,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _| {},
            &|_, _| {
                panic!("空目标会话不应回调 file_done");
            },
        )
        .unwrap();

        // 3) 会话复用 + focus 抢道：目标 f1 + f4，focus=f4 → f4 先落地
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f1.bin".into(), (0, dir.join("s1.bin")));
            st.targets.insert("f4.bin".into(), (3, dir.join("s4.bin")));
            st.focus = Some(3);
            st.alive = true;
        }
        let order = std::sync::Mutex::new(Vec::new());
        vfs.extract_session(
            &s,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _| {},
            &|idx, _| order.lock().unwrap().push(idx),
        )
        .unwrap();
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
                &|_, _| {},
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
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("n64.bin".into(), (64, d64.clone()));
            st.focus = Some(64);
            st.alive = true;
        }
        // progress 回调里模拟 UI 重定向（一次性）：跳过计数满 64（即
        // n63 消费完、n64 未到）时整体换目标——与 UI 重定向锁块同协议
        let redirected = std::sync::atomic::AtomicBool::new(false);
        vfs.extract_session(
            &s,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, stage| {
                if !stage.starts_with("跳过前置文件") {
                    return;
                }
                if redirected.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let mut st = s.lock().unwrap();
                st.targets.clear();
                st.targets.insert("n00.bin".into(), (0, d0.clone()));
                st.focus = Some(0);
                st.gen += 1;
            },
            &|idx, _| {
                assert_eq!(idx, 0, "重定向后唯一应落地的目标是 n00");
            },
        )
        .unwrap_or_else(|e| panic!("gen 救回应重新规划续跑而非误报: {e}"));
        assert_eq!(
            std::fs::read(&d0).unwrap(),
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
        {
            let mut st = s.lock().unwrap();
            st.targets.insert("f1.bin".into(), (0, dests[0].clone()));
            st.focus = Some(0);
            st.alive = true;
        }
        let done = std::sync::Mutex::new(Vec::new());
        vfs.extract_session(
            &s,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _| {},
            &|idx, _| {
                done.lock().unwrap().push(idx);
                // 落地即追加下一个目标（模拟 UI 组批）
                if idx + 1 < 4 {
                    let mut st = s.lock().unwrap();
                    st.targets.insert(
                        format!("f{}.bin", idx + 2),
                        (idx + 1, dests[idx + 1].clone()),
                    );
                }
            },
        )
        .unwrap();
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
        let order = std::sync::Mutex::new(Vec::new());
        vfs.extract_session(
            &s,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _| {},
            &|idx, _| {
                order.lock().unwrap().push(idx);
                if idx == g1 {
                    // g1 刚落地：用户跳到另一 folder 的 h1 → 漂移
                    s.lock().unwrap().focus = Some(h1);
                }
            },
        )
        .unwrap();
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
}
