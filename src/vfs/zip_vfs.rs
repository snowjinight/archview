//! ZipVFS：基于 zip crate 的随机访问 VFS（快路径）。
//!
//! zip 8.x 的 `ZipFile` 只实现 Read；`by_index_seek` 仅支持 Stored。
//! deflate 等压缩条目的 seek 由本模块实现：前向 seek = 继续解压丢弃输出；
//! 后向 seek = 重开 entry 从头解压丢弃到目标偏移（O(target)，架构文档"解码块 LRU"的未加缓存版本）。

use super::{AccessMode, ArchiveVFS, VfsError, VirtualEntry, VirtualFile};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct ZipVFS {
    path: PathBuf,
    /// 解密密码候选（UTF-8/GBK 等，见 vfs::password_candidates）；空 = 未提供密码
    password: Vec<Vec<u8>>,
    /// 打开时解析一次的 central directory 元数据。open_entry/list 用
    /// `unsafe_new_with_metadata` 免重复解析——27GB 大包在网络盘上
    /// 重复解析 CD（seek 到文件尾 + 读几 MB）代价显著
    metadata: Arc<zip::read::ZipArchiveMetadata>,
    /// CD 起始的绝对偏移（crate 已处理 ZIP64 定位与前置数据偏移）
    cd_start: u64,
    /// crate 解码的条目名快照（CD 顺序，与 by_index 下标一致）
    names: Vec<String>,
    /// 自行解码的条目名（同 CD 顺序）。crate 对无 UTF-8 标志的条目恒按
    /// CP437 解码——老工具（简中 Windows）打的 GBK 名包即乱码，且其
    /// `index_for_name` 按名字字节查内部表也必然失配（"entry 不存在"）。
    /// 故扫 CD 原始字节自行解码；None = 扫描失败/条目数与 crate 不一致
    /// （重名被折叠等边缘包），整体回退 crate 旧行为
    display_names: Option<Vec<String>>,
    /// 显示名 → crate 下标（open_entry/verify_password 查找用；
    /// 仅 display_names 为 Some 时存在）
    name_table: Option<std::collections::HashMap<String, usize>>,
}

/// zip crate 错误 → VfsError（密码缺失/错误统一映射为 Password，UI 据此弹密码框）
fn map_zip_err(e: zip::result::ZipError) -> VfsError {
    use zip::result::ZipError as Z;
    match &e {
        Z::InvalidPassword => VfsError::Password,
        Z::UnsupportedArchive(s) if *s == Z::PASSWORD_REQUIRED => VfsError::Password,
        _ => VfsError::Archive(format!("zip 操作失败: {e}")),
    }
}

/// 从 extra field 解析 ZIP64(0x0001) 的未压缩尺寸。
/// ZIP64 extra 的字段按 [未压缩尺寸, 压缩尺寸, 本地头偏移, 起始盘号] 顺序出现，
/// 且仅在对应固定字段为 0xFFFFFFFF 时存在（APPNOTE 4.5.3）——
/// 本函数只在该条件下被调用，故首 8 字节即未压缩尺寸。
fn zip64_uncompressed_size(extra: &[u8]) -> Option<u64> {
    let mut i = 0usize;
    while i + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[i], extra[i + 1]]);
        let sz = u16::from_le_bytes([extra[i + 2], extra[i + 3]]) as usize;
        if id == 0x0001 {
            let b = extra.get(i + 4..i + 12)?;
            return Some(u64::from_le_bytes(b.try_into().unwrap()));
        }
        i += 4 + sz;
    }
    None
}

/// CD 条目名解码链（老包兼容）：
/// 1. flags bit11（UTF-8 标志）置位，或字节本身是合法 UTF-8（部分工具不设
///    标志但实际 UTF-8）→ 按 UTF-8
/// 2. 配置码页（config codepage，默认 GBK）——老 WinRAR/好压按系统 ANSI
///    打包，简中即 GBK；GBK 汉字首字节多为 UTF-8 非法形态，与 1 不冲突
/// 3. 码页解出替换符（非法序列）→ lossy（极端角案：码页也解不了的西文
///    老包字节；crate 的 CP437 表未公开导出，lossy 展示 � 即可）
fn decode_cd_name(raw: &[u8], flags: u16) -> String {
    if flags & 0x800 != 0 || std::str::from_utf8(raw).is_ok() {
        return String::from_utf8_lossy(raw).into_owned();
    }
    if let Some(enc) = super::extra_encoding() {
        let (cow, _, had_errors) = enc.decode(raw);
        if !had_errors {
            return cow.into_owned();
        }
    }
    String::from_utf8_lossy(raw).into_owned()
}

/// 顺序扫一遍 CD，收集每条目的原始名字节与 flags（一次顺序读，无逐条 seek）。
/// None = 解析失败或 CD 头比 crate 条目多（重名被 crate 折叠等），
/// 对齐检查与 list_from_central_directory 同款
fn scan_cd_raw(cd_start: u64, path: &Path, expect: usize) -> Option<Vec<(Vec<u8>, u16)>> {
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::Start(cd_start)).ok()?;
    let mut out = Vec::with_capacity(expect);
    for _ in 0..expect {
        let mut hdr = [0u8; 46];
        f.read_exact(&mut hdr).ok()?;
        if &hdr[..4] != b"PK\x01\x02" {
            return None;
        }
        let flags = u16::from_le_bytes([hdr[8], hdr[9]]);
        let name_len = u16::from_le_bytes([hdr[28], hdr[29]]) as usize;
        let extra_len = u16::from_le_bytes([hdr[30], hdr[31]]) as i64;
        let cmt_len = u16::from_le_bytes([hdr[32], hdr[33]]) as i64;
        let mut raw = vec![0u8; name_len];
        f.read_exact(&mut raw).ok()?;
        f.seek(SeekFrom::Current(extra_len + cmt_len)).ok()?;
        out.push((raw, flags));
    }
    let mut peek = [0u8; 4];
    match f.read(&mut peek) {
        Ok(4) if &peek == b"PK\x01\x02" => None,
        _ => Some(out),
    }
}

impl ZipVFS {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self, VfsError> {
        let file = File::open(path).map_err(VfsError::Io)?;
        let archive = zip::read::ZipArchive::new(file)
            .map_err(|e| VfsError::Archive(format!("打开 zip 失败: {e}")))?;
        // CD 只解析这一次：元数据/CD 偏移/条目名缓存到 self，
        // 之后 open_entry 与 list 均不再重复解析（旧实现每次操作都重开重解析）
        let metadata = archive.metadata();
        let cd_start = archive.central_directory_start();
        let names = archive.file_names().map(str::to_string).collect::<Vec<_>>();
        // 扫 CD 原始名字节自行解码（见 display_names 字段注释）；
        // 失败/条目数不一致 → 整体 None，走 crate 旧行为
        let (display_names, name_table) = match scan_cd_raw(cd_start, path, names.len()) {
            Some(items) => {
                let disp: Vec<String> =
                    items.iter().map(|(r, fl)| decode_cd_name(r, *fl)).collect();
                // 显示名 → crate 下标（CD 顺序 = 插入顺序 = by_index 下标）；
                // 解码后重名（罕见）后者覆盖前者，与 crate 折叠语义近似
                let table: std::collections::HashMap<String, usize> = disp
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), i))
                    .collect();
                (Some(disp), Some(table))
            }
            None => (None, None),
        };
        Ok(Self {
            path: path.to_path_buf(),
            password: password.map(super::password_candidates).unwrap_or_default(),
            metadata,
            cd_start,
            names,
            display_names,
            name_table,
        })
    }

    /// 显示名 → crate 下标。优先自建表（GBK 等非 UTF-8 名包 crate 查不到）；
    /// 表缺失或未命中再走 crate 的 index_for_name（UTF-8 名两条路等价）
    fn index_of(&self, archive: &zip::read::ZipArchive<File>, name: &str) -> Option<usize> {
        self.name_table
            .as_ref()
            .and_then(|t| t.get(name).copied())
            .or_else(|| archive.index_for_name(name))
    }

    /// 从缓存的 CD 元数据重建 reader（不再重复解析 central directory）。
    ///
    /// # Safety
    /// metadata 解析自同一路径的同一文件（open 时）；仅当文件此后被外部改写
    /// 才会错位——压缩包在本程序的使用场景下视为不可变对象。
    fn archive(&self) -> Result<zip::read::ZipArchive<File>, VfsError> {
        let file = File::open(&self.path).map_err(VfsError::Io)?;
        Ok(unsafe { zip::read::ZipArchive::unsafe_new_with_metadata(file, self.metadata.clone()) })
    }

    /// 带（可选）密码候选按序号打开 entry（借用 archive，调用侧 transmute 延长生命周期）。
    /// 多个候选（UTF-8/GBK）依次尝试：AES/ZipCrypto 在 reader 构造时即时校验密码，
    /// InvalidPassword 说明该编码不匹配，换下一个；全部失败才报 Password。
    /// 两遍式（先探测命中候选再正式打开）：循环内直接返回借用会触发 E0499（NLL 限制）。
    fn by_index<'a>(
        archive: &'a mut zip::read::ZipArchive<File>,
        index: usize,
        candidates: &[Vec<u8>],
    ) -> Result<zip::read::ZipFile<'a, File>, VfsError> {
        if candidates.is_empty() {
            return archive
                .by_index_with_options(index, zip::read::ZipReadOptions::new())
                .map_err(map_zip_err);
        }
        // 第一遍：探测能过密码校验的候选（Ok 句柄立刻丢弃，借用随即结束）
        let mut working: Option<&[u8]> = None;
        for pw in candidates {
            let opts = zip::read::ZipReadOptions::new().password(Some(pw.as_slice()));
            match archive.by_index_with_options(index, opts) {
                Ok(_) => {
                    working = Some(pw.as_slice());
                    break;
                }
                // 该编码不匹配（或密码本身错误）：试下一个候选
                Err(zip::result::ZipError::InvalidPassword) => continue,
                Err(e) => return Err(map_zip_err(e)),
            }
        }
        let Some(pw) = working else {
            return Err(VfsError::Password);
        };
        // 第二遍：命中的候选正式打开
        let opts = zip::read::ZipReadOptions::new().password(Some(pw));
        archive
            .by_index_with_options(index, opts)
            .map_err(map_zip_err)
    }

    /// 顺序解析 central directory：每条目只取 46 字节固定头里的
    /// flags(bit0=加密) 与未压缩尺寸（ZIP64 时查 extra field），
    /// 名字以 crate 解码的 `names` 按下标对齐（避免复制其编码逻辑）。
    /// 返回 None = 与 crate 条目数不一致/解析错位，调用方走 fallback。
    /// progress 按已遍历条目/总数上报（0.0~1.0）。
    fn list_from_central_directory(&self, progress: &dyn Fn(f32)) -> Option<Vec<VirtualEntry>> {
        let mut f = File::open(&self.path).ok()?;
        f.seek(SeekFrom::Start(self.cd_start)).ok()?;
        let total = self.names.len().max(1);
        let mut out = Vec::with_capacity(self.names.len());
        for (i, name) in self.names.iter().enumerate() {
            if i % 256 == 0 {
                progress(i as f32 / total as f32);
            }
            let mut hdr = [0u8; 46]; // central directory 头固定部分
            f.read_exact(&mut hdr).ok()?;
            if &hdr[..4] != b"PK\x01\x02" {
                return None; // 错位/损坏：交给 fallback
            }
            let flags = u16::from_le_bytes([hdr[8], hdr[9]]);
            // APPNOTE 4.3.7：20-23 压缩尺寸、24-27 未压缩尺寸、
            // 28-29 名长、30-31 extra 长、32-33 注释长
            let fixed_size = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]) as u64;
            let name_len = u16::from_le_bytes([hdr[28], hdr[29]]) as i64;
            let extra_len = u16::from_le_bytes([hdr[30], hdr[31]]) as usize;
            let cmt_len = u16::from_le_bytes([hdr[32], hdr[33]]) as i64;
            // 跳过条目名（名字用 crate 的解码结果，见上）
            f.seek(SeekFrom::Current(name_len)).ok()?;
            let mut extra = vec![0u8; extra_len];
            f.read_exact(&mut extra).ok()?;
            let size = if fixed_size == 0xFFFF_FFFF {
                zip64_uncompressed_size(&extra)?
            } else {
                fixed_size
            };
            f.seek(SeekFrom::Current(cmt_len)).ok()?;
            // APPNOTE 4.3.7：16-19 CRC32（VarTidy 包内去重判据，元数据自带零解压成本）
            let crc = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
            // 名字：自解码名（GBK 等老包）优先，缺失时用 crate 解码名（同 CD 顺序对齐）
            let dname: String = match &self.display_names {
                Some(d) => d[i].clone(),
                None => name.clone(),
            };
            out.push(VirtualEntry {
                name: dname.clone(),
                size,
                is_dir: dname.ends_with('/'),
                encrypted: flags & 0b1 != 0,
                crc: Some(crc),
            });
        }
        // 数量一致性：crate 的索引表会折叠重名条目而 CD 不会——
        // 消费完 names.len() 条后若还有 CD 头剩下，说明两者错位，走 fallback
        let mut peek = [0u8; 4];
        match f.read(&mut peek) {
            Ok(4) if &peek == b"PK\x01\x02" => None,
            _ => {
                progress(1.0);
                Some(out)
            }
        }
    }

    /// 兜底：逐条 by_index_raw（每条目一次本地文件头读；仅 CD 遍历失配的边缘包走这里）。
    /// progress 同样按条目数上报。
    fn list_by_index_raw(&self, progress: &dyn Fn(f32)) -> Vec<VirtualEntry> {
        let Ok(mut zip) = self.archive() else {
            return vec![];
        };
        let total = zip.len().max(1);
        let out: Vec<VirtualEntry> = (0..zip.len())
            .filter_map(|i| {
                if i % 256 == 0 {
                    progress(i as f32 / total as f32);
                }
                // by_index_raw 不解密不解压：加密条目也能拿到元数据（文件名明文）
                zip.by_index_raw(i).ok().map(|e| VirtualEntry {
                    name: e.name().to_string(),
                    size: e.size(),
                    is_dir: e.is_dir(),
                    encrypted: e.encrypted(),
                    crc: Some(e.crc32()),
                })
            })
            .collect();
        progress(1.0);
        out
    }
}

impl ArchiveVFS for ZipVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        self.list_with_progress(&|_, _| {})
    }

    /// CD 遍历/fallback 的真实条目百分比进度（打开期间的 UI 进度条数据源）
    fn list_with_progress(&self, progress: &dyn Fn(Option<f32>, &str)) -> Vec<VirtualEntry> {
        const STAGE: &str = "正在构建文件列表";
        let cb = |f: f32| progress(Some(f), STAGE);
        // 快路径：顺序遍历 central directory（一次几 MB 的顺序读）。
        // 不能走 by_index_raw：它会为每个条目 seek 到本地文件头读一次，
        // 万级条目在网络盘/HDD 上即上万次随机读——27GB 包实测可拖到分钟级
        match self.list_from_central_directory(&cb) {
            Some(entries) => entries,
            // CD 遍历与 crate 条目数不一致（重名条目被 crate 折叠等边缘情况）：
            // 退回逐条 by_index_raw，保证与 open_entry 的下标语义一致
            None => self.list_by_index_raw(&cb),
        }
    }

    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        let mut archive = Box::new(self.archive()?);
        let index = self
            .index_of(&archive, &entry.name)
            .ok_or_else(|| VfsError::Archive(format!("entry 不存在: {}", entry.name)))?;
        let file = Self::by_index(&mut archive, index, &self.password)?;

        // 安全性：file 借用 archive（Box 内），archive 由 reader 持有且声明在 file 之后（drop 晚于 file），借用始终有效。
        let file: zip::read::ZipFile<'static, File> = unsafe { std::mem::transmute(file) };
        Ok(Box::new(SeekableZipFile {
            file: Some(file),
            archive,
            index,
            size: entry.size,
            pos: 0,
            password: self.password.clone(),
        }))
    }

    fn access_mode(&self) -> AccessMode {
        AccessMode::FastSeek
    }

    fn extract_entry(
        &self,
        entry: &VirtualEntry,
        dest: &std::path::Path,
        _cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(u64, &str),
    ) -> Result<(), VfsError> {
        let mut src = self.open_entry(entry)?;
        // 写临时文件 + 改名（与 7z/RAR 一致）：并发解压同一 dest 时不互相覆盖写坏缓存
        let tmp = super::tmp_path(dest);
        let mut out = std::fs::File::create(&tmp).map_err(|e| {
            VfsError::Archive(format!(
                "创建临时文件失败 [{}]（条目 [{}]）: {e}",
                tmp.display(),
                entry.name
            ))
        })?;
        // zip 非固态：无跳过阶段，进度直接映射（阶段恒为空 = 写目标文件）
        super::sevenz_vfs::copy_with_progress(&mut src, &mut out, entry.size, &|done| {
            progress(done, "")
        })
        .map_err(|e| {
            std::fs::remove_file(&tmp).ok(); // 清掉半截临时文件
            let e = match e {
                super::sevenz_vfs::CopyErr::Read(e) | super::sevenz_vfs::CopyErr::Write(e) => e,
            };
            VfsError::Archive(format!(
                "zip 解压失败: 压缩包 [{}] 条目 [{}] → [{}]: {e}",
                self.path.display(),
                entry.name,
                dest.display()
            ))
        })?;
        use std::io::Write;
        out.flush().map_err(VfsError::Io)?;
        std::fs::rename(&tmp, dest).map_err(|e| {
            VfsError::Archive(format!(
                "重命名临时文件失败: [{}] → [{}]: {e}",
                tmp.display(),
                dest.display()
            ))
        })?;
        Ok(())
    }

    /// 密码校验：AES/ZipCrypto 的 reader 构造即验 2 字节密码校验值（零解压），
    /// by_index 内部已按编码候选逐个尝试（InvalidPassword 换下一个），
    /// 全部失败报 Password；条目未加密时任何密码都通过
    fn verify_password(&self, entry: &VirtualEntry, password: Option<&str>) -> Result<(), VfsError> {
        let mut archive = self.archive()?;
        let index = self
            .index_of(&archive, &entry.name)
            .ok_or_else(|| VfsError::Archive(format!("entry 不存在: {}", entry.name)))?;
        let candidates = password.map(super::password_candidates).unwrap_or_default();
        Self::by_index(&mut archive, index, &candidates).map(|_| ())
    }
}

/// 可 seek 的 zip entry 读取器：archive 由自身持有。
struct SeekableZipFile {
    /// 当前解压流（None = 已越过 EOF，read 返回 0）
    file: Option<zip::read::ZipFile<'static, File>>,
    #[allow(dead_code)]
    archive: Box<zip::read::ZipArchive<File>>,
    index: usize,
    size: u64,
    pos: u64,
    /// 后向 seek 重开 entry 时解密用的密码候选
    password: Vec<Vec<u8>>,
}

/// 从 reader 丢弃 n 字节解压输出
fn skip(reader: &mut impl Read, n: u64) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = n;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let got = reader.read(&mut buf[..want])?;
        if got == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "zip entry 提前结束",
            ));
        }
        left -= got as u64;
    }
    Ok(())
}

impl Read for SeekableZipFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(f) = &mut self.file else {
            return Ok(0); // 越过 EOF 的 seek 位置：EOF
        };
        let n = f.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SeekableZipFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target_i: i64 = match pos {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::End(o) => self.size as i64 + o,
            SeekFrom::Current(o) => self.pos as i64 + o,
        };
        let target = target_i.clamp(0, i64::MAX) as u64;

        if target == self.pos {
            return Ok(self.pos);
        }
        if target > self.size {
            // 越过 EOF：置空流，read 返回 0（mpv 探测 size 时可能 seek 到末尾）
            self.file = None;
            self.pos = target;
            return Ok(target);
        }
        if target > self.pos && self.file.is_some() {
            // 前向 seek：继续解压丢弃输出
            let f = self.file.as_mut().unwrap();
            skip(f, target - self.pos)?;
            self.pos = target;
        } else {
            // 后向 seek 或无流：重开 entry 从头丢弃（加密条目带密码候选重开）
            self.file = None;
            let f = ZipVFS::by_index(&mut self.archive, self.index, &self.password)
                .map_err(std::io::Error::other)?;
            let f: zip::read::ZipFile<'static, File> = unsafe { std::mem::transmute(f) };
            self.file = Some(f);
            let f = self.file.as_mut().unwrap();
            skip(f, target)?;
            self.pos = target;
        }
        Ok(self.pos)
    }
}

impl VirtualFile for SeekableZipFile {
    fn size(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 建一个含 stored 与 deflate 条目的测试 zip（临时目录，路径不硬编码）。
    /// 路径带原子序号：多个测试并行调用时互不截断/删除对方的包
    /// （曾用固定 {pid} 路径，并行测试偶发读到被另一测试截断的半截包）
    fn make_test_zip() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zipvfs_test_{}_{n}.zip", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut w = zip::write::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("dir/a.txt", deflated).unwrap();
        w.write_all(b"hello zip vfs, this is a deflate payload!")
            .unwrap();
        w.start_file("dir/sub/b.mp4", stored).unwrap();
        w.write_all(&vec![7u8; 4096]).unwrap();
        w.finish().unwrap();
        path
    }

    /// 手工构造单条目 store zip：文件名按 GBK 编码且不设 UTF-8 标志，
    /// 模拟老 WinRAR/好压在简中 Windows 上打的包（zip crate 的 writer
    /// 无法产出这种形态——它恒设 UTF-8 标志）
    fn make_gbk_zip() -> PathBuf {
        fn crc32(data: &[u8]) -> u32 {
            let mut c = 0xFFFF_FFFFu32;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        let (name_gbk, _, had_errors) = encoding_rs::GBK.encode("荒木经惟/测试 (1).jpg");
        assert!(!had_errors);
        let content = b"jpeg-data-here";
        let crc = crc32(content);
        let mut z: Vec<u8> = Vec::new();
        // local file header（flags=0：无 UTF-8 标志 = 老工具形态）
        z.extend_from_slice(b"PK\x03\x04");
        z.extend_from_slice(&20u16.to_le_bytes()); // version needed
        z.extend_from_slice(&0u16.to_le_bytes()); // flags
        z.extend_from_slice(&0u16.to_le_bytes()); // method = store
        z.extend_from_slice(&0u16.to_le_bytes()); // dostime
        z.extend_from_slice(&0x21u16.to_le_bytes()); // dosdate = 1980-01-01
        z.extend_from_slice(&crc.to_le_bytes());
        z.extend_from_slice(&(content.len() as u32).to_le_bytes()); // csize
        z.extend_from_slice(&(content.len() as u32).to_le_bytes()); // usize
        z.extend_from_slice(&(name_gbk.len() as u16).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes()); // extra len
        z.extend_from_slice(&name_gbk);
        z.extend_from_slice(content);
        // central directory
        let cd_offset = z.len() as u32;
        z.extend_from_slice(b"PK\x01\x02");
        z.extend_from_slice(&20u16.to_le_bytes()); // version made by
        z.extend_from_slice(&20u16.to_le_bytes()); // version needed
        z.extend_from_slice(&0u16.to_le_bytes()); // flags
        z.extend_from_slice(&0u16.to_le_bytes()); // method
        z.extend_from_slice(&0u16.to_le_bytes()); // dostime
        z.extend_from_slice(&0x21u16.to_le_bytes()); // dosdate
        z.extend_from_slice(&crc.to_le_bytes());
        z.extend_from_slice(&(content.len() as u32).to_le_bytes());
        z.extend_from_slice(&(content.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name_gbk.len() as u16).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes()); // extra
        z.extend_from_slice(&0u16.to_le_bytes()); // comment
        z.extend_from_slice(&0u16.to_le_bytes()); // disk
        z.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        z.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        z.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        z.extend_from_slice(&name_gbk);
        let cd_size = z.len() as u32 - cd_offset;
        // EOCD
        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&cd_size.to_le_bytes());
        z.extend_from_slice(&cd_offset.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes()); // comment len
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zipvfs_gbk_{}.zip", std::process::id()));
        std::fs::write(&path, &z).unwrap();
        path
    }

    /// 老 GBK 名包：名字正确解码（非 CP437 乱码）+ open_entry 可读
    /// （旧 bug：crate CP437 乱码名 index_for_name 查不到 → "entry 不存在"）
    #[test]
    fn gbk_filename_decoded_and_openable() {
        let path = make_gbk_zip();
        let vfs = ZipVFS::open(&path, None).unwrap();
        let entries = vfs.list();
        let e = entries
            .iter()
            .find(|e| e.name == "荒木经惟/测试 (1).jpg")
            .expect("GBK 文件名应按 codepage 正确解码");
        let mut f = vfs.open_entry(e).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"jpeg-data-here");
    }

    #[test]
    fn list_read_seek_roundtrip() {
        let path = make_test_zip();
        let vfs = ZipVFS::open(&path, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|e| e.name == "dir/sub/b.mp4" && e.size == 4096));
        assert!(entries.iter().all(|e| !e.encrypted));

        // deflate 条目：读 + seek 往返（前向/后向）
        let e = entries
            .iter()
            .find(|e| e.name == "dir/a.txt")
            .unwrap()
            .clone();
        let mut f = vfs.open_entry(&e).unwrap();
        let mut buf = String::new();
        f.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello zip vfs, this is a deflate payload!");
        // 后向 seek（触发重开丢弃路径）
        f.seek(SeekFrom::Start(6)).unwrap();
        let mut b2 = [0u8; 3];
        f.read_exact(&mut b2).unwrap();
        assert_eq!(&b2, b"zip");
        // 前向 seek（deflate 起点 d 在 25）
        f.seek(SeekFrom::Start(25)).unwrap();
        let mut b3 = [0u8; 6];
        f.read_exact(&mut b3).unwrap();
        assert_eq!(&b3, b"deflat");
        // seek 到 EOF 之后
        let p = f.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(p, e.size);
        let mut b4 = [0u8; 4];
        assert_eq!(f.read(&mut b4).unwrap(), 0);
        std::fs::remove_file(&path).ok();
    }

    /// AES 加密 zip：列出可见（文件名明文）+ 无密码/错密码报 Password + 正确密码可读可 seek
    #[test]
    fn encrypted_zip_password_flow() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zipvfs_enc_test_{}.zip", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut w = zip::write::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .with_aes_encryption(zip::AesMode::Aes256, "pw123");
        w.start_file("secret/v.txt", opts).unwrap();
        w.write_all(b"encrypted payload for seek test, 0123456789")
            .unwrap();
        w.finish().unwrap();

        // 1) 无密码打开：列表可见且 encrypted=true
        let vfs = ZipVFS::open(&path, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].encrypted);
        // 2) 无密码读 → Password
        let e = entries[0].clone();
        assert!(matches!(vfs.open_entry(&e), Err(VfsError::Password)));
        // 3) 错密码读 → Password（AES 打开时即时校验）
        let vfs_bad = ZipVFS::open(&path, Some("wrong")).unwrap();
        assert!(matches!(vfs_bad.open_entry(&e), Err(VfsError::Password)));
        // 4) 正确密码：读 + 后向 seek（重开路径要带密码解密）
        let vfs_ok = ZipVFS::open(&path, Some("pw123")).unwrap();
        let mut f = vfs_ok.open_entry(&e).unwrap();
        let mut buf = String::new();
        f.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "encrypted payload for seek test, 0123456789");
        f.seek(SeekFrom::Start(10)).unwrap();
        let mut b = [0u8; 3];
        f.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"pay");
        std::fs::remove_file(&path).ok();
    }

    /// 非 ASCII 密码（汉字/特殊字符）：UTF-8 候选路径 + 候选重试兜底
    /// （WinRAR 等老工具按 GBK 编码 zip 密码；候选机制保证两种编码都能解）
    #[test]
    fn encrypted_zip_non_ascii_password() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zipvfs_cjk_pw_{}.zip", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut w = zip::write::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .with_aes_encryption(zip::AesMode::Aes256, "密码测试!@#123");
        w.start_file("secret/中文.mp4", opts).unwrap();
        w.write_all(b"non-ascii password payload").unwrap();
        w.finish().unwrap();

        // 汉字密码：候选 = [UTF-8, GBK]，UTF-8 命中（zip crate 写入按 UTF-8）
        let vfs = ZipVFS::open(&path, Some("密码测试!@#123")).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].encrypted);
        let mut f = vfs.open_entry(&entries[0]).unwrap();
        let mut buf = String::new();
        f.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "non-ascii password payload");
        // 后向 seek：重开路径带候选重试
        f.seek(SeekFrom::Start(4)).unwrap();
        let mut b = [0u8; 5];
        f.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ascii");
        // 错密码（编码正确但内容错误）：两个候选都失败 → Password
        let vfs_bad = ZipVFS::open(&path, Some("错误密码测试")).unwrap();
        assert!(matches!(
            vfs_bad.open_entry(&entries[0]),
            Err(VfsError::Password)
        ));
        std::fs::remove_file(&path).ok();
    }

    /// CD 顺序遍历（list 快路径）与逐条 by_index_raw（fallback）结果一致：
    /// 名字/尺寸/目录标记/加密标记逐字段对齐，保证快路径没有静默漂移
    #[test]
    fn cd_walk_matches_by_index_raw() {
        let path = make_test_zip();
        let vfs = ZipVFS::open(&path, None).unwrap();
        // 快路径确实可用（没悄悄落回 fallback）
        let fast = vfs
            .list_from_central_directory(&|_| {})
            .expect("CD 遍历应成功");
        let slow = vfs.list_by_index_raw(&|_| {});
        assert_eq!(fast.len(), slow.len());
        for (f, s) in fast.iter().zip(slow.iter()) {
            assert_eq!(f.name, s.name);
            assert_eq!(f.size, s.size);
            assert_eq!(f.is_dir, s.is_dir);
            assert_eq!(f.encrypted, s.encrypted);
            assert_eq!(f.crc, s.crc, "CD 遍历与 fallback 的 CRC 应一致");
        }
        // list() 走的就是快路径结果（长度一致；字段已在上面与 fallback 对齐校验）
        assert_eq!(vfs.list().len(), fast.len());
        std::fs::remove_file(&path).ok();
    }

    /// list_with_progress：进度单调递增、以 1.0 收尾，结果与 list() 一致
    #[test]
    fn list_with_progress_reports() {
        let path = make_test_zip();
        let vfs = ZipVFS::open(&path, None).unwrap();
        // &dyn Fn 回调不能捕获可变状态，用 Cell 记录
        let (last, saw_done) = (std::cell::Cell::new(0.0f32), std::cell::Cell::new(false));
        let entries = vfs.list_with_progress(&|f, stage| {
            let f = f.expect("ZipVFS 的 list_with_progress 应上报真实百分比");
            assert_eq!(stage, "正在构建文件列表");
            assert!(f >= last.get(), "进度应单调递增");
            last.set(f);
            if f >= 1.0 {
                saw_done.set(true);
            }
        });
        assert!(saw_done.get(), "应以 1.0 收尾");
        assert_eq!(entries.len(), vfs.list().len());
        std::fs::remove_file(&path).ok();
    }

    /// ZIP64 extra(0x0001) 未压缩尺寸解析：字段顺序 [usize, csize, offset, disk]
    #[test]
    fn zip64_extra_size_parse() {
        let mk = |id: u16, data: &[u8]| {
            let mut v = Vec::new();
            v.extend_from_slice(&id.to_le_bytes());
            v.extend_from_slice(&(data.len() as u16).to_le_bytes());
            v.extend_from_slice(data);
            v
        };
        // 前面放一个无关 extra（如 NTFS 时间戳 0x000a），后面 ZIP64
        let mut extra = mk(0x000a, &[0u8; 32]);
        let mut z64 = 0xDEAD_BEEFu64.to_le_bytes().to_vec();
        z64.extend_from_slice(&0xABCD_EF01u64.to_le_bytes()); // csize（顺序跳过）
        z64.extend_from_slice(&0x1234_5678u64.to_le_bytes()); // 本地头偏移（顺序跳过）
        extra.extend_from_slice(&mk(0x0001, &z64));
        assert_eq!(zip64_uncompressed_size(&extra), Some(0xDEAD_BEEF));
        // 无 ZIP64 extra
        assert_eq!(zip64_uncompressed_size(&mk(0x000a, &[1, 2, 3])), None);
        // ZIP64 数据不足 8 字节（损坏包）
        assert_eq!(zip64_uncompressed_size(&mk(0x0001, &[1, 2, 3])), None);
    }
}
