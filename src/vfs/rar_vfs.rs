//! RarVFS：RAR 格式（固态慢路径，unrar crate 内嵌编译 UnRAR 源码，支持 RAR4/5、分卷）。
//! 与 7z 同理：后台解压到缓存目录 → 播放本地文件。

use super::{AccessMode, ArchiveVFS, VirtualEntry, VirtualFile, VfsError};
use std::path::{Path, PathBuf};
use unrar::Archive;

pub struct RarVFS {
    path: PathBuf,
    /// 解密密码候选（UTF-8/GBK 等，见 vfs::password_candidates）；空 = 未提供密码。
    /// 加密头包在 open 阶段就会解析出唯一正确候选（见 open）。
    password: Vec<Vec<u8>>,
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
                match candidates.iter().find(|c| try_list(Some(c.as_slice())).is_ok()) {
                    Some(pw) => candidates = vec![pw.clone()],
                    None => return Err(VfsError::Password),
                }
            }
            Err(e) => return Err(VfsError::Archive(format!("打开 RAR 失败: {e}"))),
        }
        Ok(Self {
            path: first.to_path_buf(),
            password: candidates,
        })
    }

    /// listing 用（头未加密时任何候选/无密码均可）
    fn archive(&self) -> Archive<'_> {
        match self.password.first() {
            Some(pw) => Archive::with_password(&self.path, pw.as_slice()),
            None => Archive::new(&self.path),
        }
    }

    /// 单个密码候选的解压尝试：定位条目 + extract_to 临时文件（不改名）
    fn try_extract(
        &self,
        pw: Option<&[u8]>,
        entry: &VirtualEntry,
        tmp: &Path,
    ) -> Result<(), VfsError> {
        let arch = match pw {
            Some(p) => Archive::with_password(&self.path, p),
            None => Archive::new(&self.path),
        };
        let mut open = arch.open_for_processing().map_err(|e| {
            VfsError::Archive(format!("打开 RAR 失败 [{}]: {e}", self.path.display()))
        })?;
        loop {
            match open
                .read_header()
                .map_err(|e| VfsError::Archive(format!("读 RAR 头失败 [{}]: {e}", self.path.display())))?
            {
                Some(head) => {
                    let name = head
                        .entry()
                        .filename
                        .to_string_lossy()
                        .replace('\\', "/");
                    if name == entry.name {
                        if head.entry().is_encrypted() && pw.is_none() {
                            return Err(VfsError::Password);
                        }
                        head.extract_to(tmp).map_err(|e| {
                            if is_password_err(&e) {
                                VfsError::Password
                            } else {
                                VfsError::Archive(format!(
                                    "RAR 解压失败: 压缩包 [{}] 条目 [{}] 解压到 [{}] 失败: {e}",
                                    self.path.display(),
                                    entry.name,
                                    tmp.display()
                                ))
                            }
                        })?;
                        return Ok(());
                    } else {
                        open = head.skip().map_err(|e| {
                            VfsError::Archive(format!(
                                "RAR 跳过条目 [{}] 失败（正在定位目标 [{}]）: {e}",
                                name, entry.name
                            ))
                        })?;
                    }
                }
                None => {
                    return Err(VfsError::Archive(format!(
                        "entry 不存在: {}",
                        entry.name
                    )))
                }
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
            "RAR 固态格式不支持流式读取，请走后台解压路径".into(),
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
        // 先解压到同目录临时文件再改名，避免中断留下半截文件被当成有效缓存；
        // 临时文件名带序号：同一条目被并发解压时不会互相抢同一个 .part（Windows 独占打开冲突）
        let tmp = super::tmp_path(dest);
        // 尝试序列：有密码 → 各编码候选，最后兜底无密码（条目实际未加密时直接成功）；
        // 无密码 → 仅无密码（加密条目在 try_extract 内报 Password，UI 弹密码框）
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
            match self.try_extract(pw.as_deref(), entry, &tmp) {
                Ok(()) => {
                    std::fs::rename(&tmp, dest).map_err(|e| {
                        VfsError::Archive(format!(
                            "重命名临时文件失败: [{}] → [{}]: {e}",
                            tmp.display(),
                            dest.display()
                        ))
                    })?;
                    progress(entry.size);
                    return Ok(());
                }
                // 密码/编码不匹配（RAR4 无密码哈希，错密码可能只报 CRC 失败）：换下一候选
                Err(e) if is_pwd_or_crc(&e) => {
                    std::fs::remove_file(&tmp).ok(); // 清掉半截临时文件
                    last_err = e;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err)
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
        vfs.extract_entry(&entries[0], &dest, &|_| {}).unwrap();
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len(), 50_000);
        assert!(data.iter().all(|&b| b == 9));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
