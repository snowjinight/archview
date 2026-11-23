//! 单文件 VFS：慢路径（7z/RAR 落盘）raw bit_shift 复用 zvf:// 流层的桥梁。
//!
//! 慢路径解压缓存文件是忠实拷贝（位移不落盘、不进缓存键）；需要 bit_shift
//! 修正播放时，把单个落盘文件包装成单条目 FastSeek VFS 注册进 stream_cb
//! 注册表，经 zvf:// 装载——mpv 的 read/seek 回调走 open_cb → open_entry →
//! 本地文件直读，`s=N` 时在流层包 ShiftReader（与 zip 快路径同源实现）。
//! DirVFS（目录模式=本地文件走 zvf:// 快路径）的既有先例，此处只是把
//! "目录"缩成"一个文件"。

use std::path::PathBuf;

use super::{AccessMode, ArchiveVFS, VfsError, VirtualEntry, VirtualFile};

/// 把一个已落盘的本地文件包装成单条目 VFS（条目名 = 文件名）。
pub struct SingleFileVFS {
    path: PathBuf,
    entry: VirtualEntry,
}

impl SingleFileVFS {
    /// metadata 失败（文件不存在/不可读）返回 Err，调用方兜底走 load_path
    /// 旧行为。size 在构造时取一次——同一路径重新解压后同尺寸缓存命中
    /// （start_extract 的 len==size 判据），注册项经同路径重开覆盖，条目
    /// 大小不会失真。
    pub fn open(path: PathBuf) -> Result<Self, VfsError> {
        let md = std::fs::metadata(&path)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            entry: VirtualEntry {
                name,
                size: md.len(),
                is_dir: false,
                encrypted: false,
                crc: None,
            },
            path,
        })
    }
}

impl ArchiveVFS for SingleFileVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        vec![self.entry.clone()]
    }

    /// 本地文件直开：size 打开时取一次 metadata 缓存（同 DirVFS），LRU
    /// 驱逐后重开注册项即拿到新句柄——无跨 loadfile 的文件状态
    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        if entry.name != self.entry.name {
            return Err(VfsError::Archive(format!("条目不在集合中: {}", entry.name)));
        }
        let f = std::fs::File::open(&self.path)?;
        let len = f.metadata()?.len();
        Ok(Box::new(super::dir_vfs::FsFile { f, len }))
    }

    /// 本地文件 seek O(1)：与 DirVFS 同待遇，走 mpv 流式快路径
    fn access_mode(&self) -> AccessMode {
        AccessMode::FastSeek
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    fn 单文件vfs_列表打开与尺寸() {
        let dir = std::env::temp_dir().join(format!(
            "file_vfs_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("frame_64x32.raw");
        std::fs::write(&p, [0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();

        let vfs = SingleFileVFS::open(p.clone()).unwrap();
        // list：单条目，名 = 文件名，size = 文件实长
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "frame_64x32.raw");
        assert_eq!(entries[0].size, 8);
        assert!(!entries[0].is_dir && !entries[0].encrypted);
        assert_eq!(entries[0].crc, None);
        // access_mode：FastSeek（mpv 流式快路径待遇）
        assert_eq!(vfs.access_mode(), AccessMode::FastSeek);

        // open_entry：读 + seek 全程可用，size 与文件实长一致
        let mut f = vfs.open_entry(&entries[0]).unwrap();
        assert_eq!(f.size(), 8);
        let mut buf = [0u8; 4];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0, 1, 2, 3]);
        f.seek(SeekFrom::Start(6)).unwrap();
        f.read_exact(&mut buf[..2]).unwrap();
        assert_eq!(&buf[..2], &[6, 7]);

        // 未知条目拒绝（防串号）
        let bogus = VirtualEntry {
            name: "other.bin".into(),
            size: 8,
            is_dir: false,
            encrypted: false,
            crc: None,
        };
        assert!(vfs.open_entry(&bogus).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 单文件vfs_文件不存在报错() {
        let p = std::env::temp_dir().join(format!("file_vfs_missing_definitely_{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert!(SingleFileVFS::open(p).is_err());
    }
}
