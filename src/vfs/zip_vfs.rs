//! ZipVFS：基于 zip crate 的随机访问 VFS（快路径）。
//!
//! zip 8.x 的 `ZipFile` 只实现 Read；`by_index_seek` 仅支持 Stored。
//! deflate 等压缩条目的 seek 由本模块实现：前向 seek = 继续解压丢弃输出；
//! 后向 seek = 重开 entry 从头解压丢弃到目标偏移（O(target)，架构文档"解码块 LRU"的未加缓存版本）。

use super::{AccessMode, ArchiveVFS, VirtualEntry, VirtualFile, VfsError};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct ZipVFS {
    path: PathBuf,
    /// 解密密码候选（UTF-8/GBK 等，见 vfs::password_candidates）；空 = 未提供密码
    password: Vec<Vec<u8>>,
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

impl ZipVFS {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self, VfsError> {
        let file = File::open(path).map_err(VfsError::Io)?;
        zip::read::ZipArchive::new(file)
            .map_err(|e| VfsError::Archive(format!("打开 zip 失败: {e}")))?;
        Ok(Self {
            path: path.to_path_buf(),
            password: password.map(super::password_candidates).unwrap_or_default(),
        })
    }

    fn reopen(&self) -> Result<zip::read::ZipArchive<File>, VfsError> {
        let file = File::open(&self.path).map_err(VfsError::Io)?;
        zip::read::ZipArchive::new(file)
            .map_err(|e| VfsError::Archive(format!("重开 zip 失败: {e}")))
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
        archive.by_index_with_options(index, opts).map_err(map_zip_err)
    }
}

impl ArchiveVFS for ZipVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        let Ok(mut zip) = self.reopen() else {
            return vec![];
        };
        (0..zip.len())
            .filter_map(|i| {
                // by_index_raw 不解密不解压：加密条目也能拿到元数据（文件名明文）
                zip.by_index_raw(i).ok().map(|e| VirtualEntry {
                    name: e.name().to_string(),
                    size: e.size(),
                    is_dir: e.is_dir(),
                    encrypted: e.encrypted(),
                })
            })
            .collect()
    }

    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        let mut archive = Box::new(self.reopen()?);
        let index = archive
            .index_for_name(&entry.name)
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
        progress: &dyn Fn(u64),
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
        super::sevenz_vfs::copy_with_progress(&mut src, &mut out, entry.size, progress).map_err(
            |e| {
                std::fs::remove_file(&tmp).ok(); // 清掉半截临时文件
                VfsError::Archive(format!(
                    "zip 解压失败: 压缩包 [{}] 条目 [{}] → [{}]: {e}",
                    self.path.display(),
                    entry.name,
                    dest.display()
                ))
            },
        )?;
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

    /// 建一个含 stored 与 deflate 条目的测试 zip（临时目录，路径不硬编码）
    fn make_test_zip() -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zipvfs_test_{}.zip", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut w = zip::write::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("dir/a.txt", deflated).unwrap();
        w.write_all(b"hello zip vfs, this is a deflate payload!").unwrap();
        w.start_file("dir/sub/b.mp4", stored).unwrap();
        w.write_all(&vec![7u8; 4096]).unwrap();
        w.finish().unwrap();
        path
    }

    #[test]
    fn list_read_seek_roundtrip() {
        let path = make_test_zip();
        let vfs = ZipVFS::open(&path, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|e| e.name == "dir/sub/b.mp4" && e.size == 4096)
        );
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
        w.write_all(b"encrypted payload for seek test, 0123456789").unwrap();
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
        assert!(matches!(vfs_bad.open_entry(&entries[0]), Err(VfsError::Password)));
        std::fs::remove_file(&path).ok();
    }
}
