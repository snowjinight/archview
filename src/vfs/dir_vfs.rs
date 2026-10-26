//! 目录 VFS：把磁盘目录当作媒体集合打开（递归收集图片/视频，跳过压缩包等
//! 其它文件）。ARCHITECTURE.md 4.2「目录 = 媒体集合」新语义——与拖放/打开
//! 目录对话框/命令行参数共用同一条规则。
//!
//! `access_mode = FastSeek`：本地文件随机访问 O(1)，与 ZipVFS 同走 mpv
//! zvf:// 流式快路径——解压会话/滑窗/缓存/密码等慢路径机制一概不涉及
//! （trait 默认实现的 `extract_*`/`verify_password` 永远不会被调用：条目
//! 全部 `encrypted=false`，密码探测零成本直通）。

use super::{ArchiveVFS, AccessMode, VirtualEntry, VirtualFile, VfsError};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 递归深度上限：符号链接/junction 已被跳过不存在环，此为异常深树的栈保险
const MAX_DEPTH: usize = 64;

pub struct DirVFS {
    /// 集合根目录
    root: PathBuf,
    /// 全部媒体条目，顺序 = 路径字典序（read_dir 顺序不保证，必须排序——
    /// `ui.sort_files=false` 默认按条目顺序显示，目录集合没有"包内物理
    /// 顺序"可言，字典序是最可预期的确定性顺序）
    entries: Vec<VirtualEntry>,
    /// 条目名 → 磁盘完整路径（open_entry 查找）。条目名 = root 下相对路径，
    /// 统一 '/' 分隔，与压缩包条目名形态一致（列表显示/搜索/扩展名判定一致）
    paths: HashMap<String, PathBuf>,
}

impl DirVFS {
    /// 打开目录为媒体集合。`exts` = 媒体扩展名（不带点，不分大小写）。
    /// - 根目录不可读 → `Err(Io)`（定性失败）
    /// - 子目录不可读 / 条目 stat 失败 → 静默跳过（部分可读的树照常收集）
    /// - 无媒体文件 → `Ok`（空列表），由 UI 在打开完成时统一提示
    pub fn open(root: &Path, exts: &[String]) -> Result<Self, VfsError> {
        // 根目录可读性在此定性：不可读直接失败，错误文案与压缩包打开一致
        std::fs::read_dir(root)?;
        let mut vfs = DirVFS {
            root: root.to_path_buf(),
            entries: Vec::new(),
            paths: HashMap::new(),
        };
        vfs.walk(root, exts, 0);
        Ok(vfs)
    }

    fn walk(&mut self, dir: &Path, exts: &[String], depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<_> = rd.flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for e in children {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                self.walk(&p, exts, depth + 1);
            } else if ft.is_file() {
                // 扩展名过滤（小写比较）：压缩包/字幕等其它文件天然不在集合内
                let ext = p
                    .extension()
                    .and_then(|x| x.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !exts.iter().any(|x| x.eq_ignore_ascii_case(&ext)) {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let Ok(rel) = p.strip_prefix(&self.root) else { continue };
                let name = rel.to_string_lossy().replace('\\', "/");
                self.paths.insert(name.clone(), p);
                self.entries.push(VirtualEntry {
                    name,
                    size: md.len(),
                    is_dir: false,
                    encrypted: false,
                    crc: None,
                });
            }
            // 符号链接 / junction（非 dir 非 file）一律跳过：防环且语义可预期
        }
    }
}

impl ArchiveVFS for DirVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        self.entries.clone()
    }

    /// 本地文件直开：size 在打开时取一次 metadata 缓存，size() 回调零成本
    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        let path = self
            .paths
            .get(&entry.name)
            .ok_or_else(|| VfsError::Archive(format!("条目不在集合中: {}", entry.name)))?;
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        Ok(Box::new(FsFile { f, len }))
    }

    /// 本地文件 seek O(1)——走 mpv 流式快路径（与 ZipVFS 同待遇）
    fn access_mode(&self) -> AccessMode {
        AccessMode::FastSeek
    }
}

/// `std::fs::File` 的 `VirtualFile` 适配（只读 + seek，size 打开时缓存）
struct FsFile {
    f: std::fs::File,
    len: u64,
}

impl Read for FsFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.f.read(buf)
    }
}

impl Seek for FsFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.f.seek(pos)
    }
}

impl VirtualFile for FsFile {
    fn size(&self) -> u64 {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建测试目录树（临时目录 + 原子序号，与 zip_vfs/sevenz_vfs 测试同惯例：
    /// 多测试并行时互不干扰）。返回根路径与已写入文件的 (相对路径, 内容) 表
    fn make_tree() -> (PathBuf, Vec<(String, Vec<u8>)>) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "dirvfs_test_{}_{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&root);
        let files: Vec<(String, Vec<u8>)> = vec![
            ("a.mp4".into(), vec![1; 100]),
            ("b.MP4".into(), vec![2; 200]), // 大写扩展名同样命中
            ("sub/c.jpg".into(), vec![3; 300]),
            ("sub/deep/d.mkv".into(), vec![4; 400]),
            ("notes.txt".into(), b"skip me".to_vec()), // 非媒体
            ("movie.zip".into(), vec![0; 50]),         // 压缩包跳过（目录集合只收媒体）
        ];
        for (rel, data) in &files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, data).unwrap();
        }
        (root, files)
    }

    fn exts() -> Vec<String> {
        ["mp4", "jpg", "mkv"].iter().map(|s| s.to_string()).collect()
    }

    fn find<'a>(files: &'a [(String, Vec<u8>)], rel: &str) -> &'a (String, Vec<u8>) {
        files.iter().find(|(r, _)| r == rel).unwrap()
    }

    #[test]
    fn walk_collects_media_recursively_deterministic_order_skips_non_media() {
        let (root, files) = make_tree();
        let vfs = DirVFS::open(&root, &exts()).unwrap();
        let list = vfs.list();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        // 字典序确定；相对路径 '/' 分隔；非媒体与压缩包均被过滤
        assert_eq!(names, ["a.mp4", "b.MP4", "sub/c.jpg", "sub/deep/d.mkv"]);
        for e in &list {
            let (_, data) = find(&files, &e.name);
            assert_eq!(e.size, data.len() as u64);
            assert!(!e.is_dir && !e.encrypted && e.crc.is_none());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn open_entry_reads_and_seeks_correct_file() {
        let (root, files) = make_tree();
        let vfs = DirVFS::open(&root, &exts()).unwrap();
        let (rel, data) = find(&files, "sub/deep/d.mkv");
        let entry = VirtualEntry {
            name: rel.clone(),
            size: data.len() as u64,
            is_dir: false,
            encrypted: false,
            crc: None,
        };
        let mut vf = vfs.open_entry(&entry).unwrap();
        assert_eq!(vf.size(), 400);
        // 顺序读
        let mut buf = Vec::new();
        vf.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, *data);
        // seek 后读（mpv 快路径的典型用法）
        vf.seek(SeekFrom::Start(2)).unwrap();
        let mut head = [0u8; 3];
        vf.read_exact(&mut head).unwrap();
        assert_eq!(&head, &data[2..5]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unknown_entry_errors_and_empty_dir_yields_empty_collection() {
        let (root, _) = make_tree();
        let vfs = DirVFS::open(&root, &exts()).unwrap();
        let ghost = VirtualEntry {
            name: "ghost.mp4".into(),
            size: 0,
            is_dir: false,
            encrypted: false,
            crc: None,
        };
        assert!(vfs.open_entry(&ghost).is_err());

        let empty = root.join("empty_dir_不存在的子树");
        std::fs::create_dir_all(&empty).unwrap();
        let vfs2 = DirVFS::open(&empty, &exts()).unwrap();
        assert!(vfs2.list().is_empty());
        // empty 在 root 内部，随 root 一起删除
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_root_errors() {
        // 不存在的路径：read_dir 失败定性为打开失败（与压缩包路径不存在同待遇）
        let missing = std::env::temp_dir().join("dirvfs_missing_root_不存在");
        assert!(DirVFS::open(&missing, &exts()).is_err());
    }
}
