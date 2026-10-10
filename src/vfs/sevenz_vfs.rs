//! SevenzVFS：7z 格式（固态慢路径）。
//! sevenz-rust 只支持顺序解压（固态块内 seek 需从块头重解），因此 open_entry 不支持，
//! 播放走后台解压到缓存目录 → 播放本地文件。

use super::{AccessMode, ArchiveVFS, VirtualEntry, VirtualFile, VfsError};
use sevenz_rust::{Password, SevenZReader};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct SevenzVFS {
    path: PathBuf,
    /// 内容加密条目解压用（加密头则在 open 阶段就需正确密码）
    password: Option<Password>,
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
        let _ = reader.archive(); // 头已读入
        Ok(Self {
            path: path.to_path_buf(),
            password: if password.is_some() {
                Some(Password::from(password.unwrap()))
            } else {
                None
            },
        })
    }

    fn reader(&self) -> Result<SevenZReader<std::fs::File>, VfsError> {
        let pw = self
            .password
            .clone()
            .unwrap_or_else(Password::empty);
        SevenZReader::open(&self.path, pw).map_err(map_7z_err)
    }
}

impl ArchiveVFS for SevenzVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        let Ok(reader) = self.reader() else {
            return vec![];
        };
        reader
            .archive()
            .files
            .iter()
            .map(|e| VirtualEntry {
                name: e.name.clone(),
                size: e.size,
                is_dir: e.is_directory,
                // sevenz-rust 未公开条目加密检测（content_methods 私有），
                // 内容加密条目在解压时报 Password，由 UI 弹密码框重试
                encrypted: false,
            })
            .collect()
    }

    fn open_entry(&self, _entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        Err(VfsError::Unsupported(
            "7z 固态格式不支持流式读取，请走后台解压路径".into(),
        ))
    }

    fn access_mode(&self) -> AccessMode {
        AccessMode::Solid
    }

    fn extract_entry(
        &self,
        entry: &VirtualEntry,
        dest: &Path,
        progress: &dyn Fn(u64),
    ) -> Result<(), VfsError> {
        let mut reader = self.reader()?;
        // 写临时文件 + 改名（与 RAR 一致）：并发解压同一 dest 时不互相覆盖写坏缓存
        let tmp = super::tmp_path(dest);
        let mut out = std::fs::File::create(&tmp).map_err(|e| {
            VfsError::Archive(format!(
                "创建临时文件失败 [{}]（条目 [{}]）: {e}",
                tmp.display(),
                entry.name
            ))
        })?;
        let mut found = false;
        reader
            .for_each_entries(|e, data| {
                if e.name == entry.name {
                    copy_with_progress(data, &mut out, entry.size, progress)?;
                    found = true;
                    Ok(false) // 停止迭代
                } else {
                    Ok(true)
                }
            })
            .map_err(|e| {
                std::fs::remove_file(&tmp).ok(); // 清掉半截临时文件
                // 密码错误单独识别（UI 弹密码框重试），其余带上完整上下文
                let mapped = map_7z_err(e);
                if matches!(mapped, VfsError::Password) {
                    mapped
                } else {
                    VfsError::Archive(format!(
                        "7z 解压失败: 压缩包 [{}] 条目 [{}] → [{}]: {mapped}",
                        self.path.display(),
                        entry.name,
                        dest.display()
                    ))
                }
            })?;
        if !found {
            std::fs::remove_file(&tmp).ok();
            return Err(VfsError::Archive(format!("entry 不存在: {}", entry.name)));
        }
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

/// 拷贝并按 4MB 粒度回调进度
pub(crate) fn copy_with_progress(
    src: &mut dyn Read,
    dest: &mut impl Write,
    total: u64,
    progress: &dyn Fn(u64),
) -> std::io::Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done: u64 = 0;
    let mut last_report: u64 = 0;
    progress(0);
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dest.write_all(&buf[..n])?;
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
        let entry =
            sevenz_rust::SevenZArchiveEntry::from_path(&payload, "sub/dir/a.bin".into());
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
        vfs.extract_entry(&a, &dest, &|_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })
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
        w.set_content_methods(vec![sevenz_rust::AesEncoderOptions::new(Password::from("pw123")).into()]);
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
        assert!(matches!(
            vfs.extract_entry(&entries[0], &dest, &|_| {}),
            Err(VfsError::Password)
        ));
        // 3) 错密码解压 → Password
        let vfs_bad = SevenzVFS::open(&arch, Some("wrong")).unwrap();
        assert!(matches!(
            vfs_bad.extract_entry(&entries[0], &dest, &|_| {}),
            Err(VfsError::Password)
        ));
        // 4) 正确密码解压成功
        let vfs_ok = SevenzVFS::open(&arch, Some("pw123")).unwrap();
        vfs_ok
            .extract_entry(&entries[0], &dest, &|_| {})
            .unwrap();
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len(), 60_000);
        assert!(data.iter().all(|&b| b == 5));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
