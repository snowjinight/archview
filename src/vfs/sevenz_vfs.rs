//! SevenzVFS：7z 格式（固态慢路径）。
//! sevenz-rust 只支持顺序解压（固态块内 seek 需从块头重解），因此 open_entry 不支持，
//! 播放走后台解压到缓存目录 → 播放本地文件。

use super::{AccessMode, ArchiveVFS, VfsError, VirtualEntry, VirtualFile};
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
    /// 向前预取免费区 = [folder 首条目, 目标)（locate_start）
    folder_of: Vec<Option<usize>>,
    folder_first: std::collections::HashMap<usize, usize>,
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
        // folder 布局（locate_start 的向前预取免费区判据）：条目 → 所在 folder、
        // folder → 首个条目（首个条目的 locate_start 即其自身，免费区为空）
        let folder_of: Vec<Option<usize>> = archive
            .stream_map
            .file_folder_index
            .iter()
            .copied()
            .collect();
        let mut folder_first = std::collections::HashMap::new();
        for (i, f) in folder_of.iter().enumerate() {
            if let Some(f) = f {
                folder_first.entry(*f).or_insert(i);
            }
        }
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
        AccessMode::Solid
    }

    /// 向前预取免费区 = 目标所在 folder 的 [首条目, 目标)：定位目标时
    /// 这段数据必然被顺序解掉丢弃（BlockDecoder 从 folder 头解码到目标），
    /// 顺带写盘零解码增量。跨 folder 不免费（只 seek 不解压）——
    /// 向前预取不得超出 folder 边界（用户配置的预算可能大于 folder 前缀，实取以边界为准）
    fn locate_start(&self, entry_idx: usize) -> Option<usize> {
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
                    let done = (|| -> Result<(), sevenz_rust::Error> {
                        // 写临时文件 + 改名：并发解压同一 dest 时不互相覆盖写坏缓存
                        let tmp = super::tmp_path(dest);
                        let res = (|| -> Result<(), sevenz_rust::Error> {
                            let mut out = std::fs::File::create(&tmp).map_err(|err| {
                                sevenz_rust::Error::other(format!(
                                    "创建临时文件失败 [{}]: {err}",
                                    tmp.display()
                                ))
                            })?;
                            copy_with_progress(data, &mut out, e.size, &|done| progress(done, ""))
                                .map_err(|err| match err {
                                    // 读侧失败（解码/CRC）：保留 io 形态，sevenz-rust 在
                                    // maybe_bad_password 里会把它转成 MaybeBadPassword →
                                    // VfsError::Password（UI 弹密码框重试的关键路径）
                                    CopyErr::Read(e) => sevenz_rust::Error::io(e),
                                    CopyErr::Write(e) => sevenz_rust::Error::other(format!(
                                        "写入临时文件失败 [{}]: {e}",
                                        tmp.display()
                                    )),
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
                    })();
                    match done {
                        Ok(()) => {
                            remaining.remove(e.name.as_str());
                            file_done(ti);
                        }
                        Err(e) => return Err(e),
                    }
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

    /// 密码校验：7z 头不存储密码校验值，唯一可靠的判据是真解压 + CRC——
    /// 用 BlockDecoder folder 直达目标条目，解压到内存 sink（零落盘），
    /// 错密码必致 CRC 失败，sevenz-rust 在密码非空时将其转报
    /// MaybeBadPassword → Password。目标之前的同 folder 条目必须顺序消费
    /// （solid 流不可跳），故探测目标应选最小加密条目
    fn verify_password(&self, entry: &VirtualEntry, password: Option<&str>) -> Result<(), VfsError> {
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

    /// locate_start（向前预取免费区）：solid 单 folder 包 → 前缀整段免费；
    /// 多 folder 包 → 免费区从目标所在 folder 首条目起，不跨 folder
    /// （向前预取不得把前置 folder 的条目当免费数据）
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
            "向前预取免费区不得超出目标所在 folder"
        );
        // 各 folder 首条目：免费区为空（起点即自身）
        assert_eq!(vfs.locate_start(0), Some(0));
        assert_eq!(vfs.locate_start(2), Some(2));
        // 越界/无数据流条目：None（不 panic）
        assert_eq!(vfs.locate_start(4), None);
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
}
