//! VFS 层：统一压缩包读取接口。
//! Phase 1 只实现 ZipVFS；7z/RAR 是后续 Phase，接口不变。

pub mod sevenz_vfs;
pub mod zip_vfs;
pub mod rar_vfs;

use std::fmt;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 解压临时文件名自增序号：同一 dest 被并发解压（双击/孤儿线程）时互不踩踏
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 密码候选代码页（启动时由 config.json 的 password_codepage 初始化；
/// 未初始化时默认 GBK，便于单元测试直接跑）
static CODEPAGE: std::sync::OnceLock<Option<&'static encoding_rs::Encoding>> =
    std::sync::OnceLock::new();

/// 设置密码候选代码页（WHATWG 标签："gbk"/"big5"/"shift_jis"/"euc-kr"…；
/// "none"/空 = 只试 UTF-8；utf-8 本身是默认候选无需追加；未知标签告警并保持默认）
pub fn set_password_codepage(name: &str) {
    let enc = if name.is_empty() || name.eq_ignore_ascii_case("none") {
        None
    } else {
        match encoding_rs::Encoding::for_label(name.as_bytes()) {
            Some(e) if e != encoding_rs::UTF_8 => Some(e),
            Some(_) => None, // utf-8：默认候选已有，无需追加
            None => {
                tracing::warn!("未知密码代码页 [{name}]，保持默认 GBK");
                return;
            }
        }
    };
    let _ = CODEPAGE.set(enc);
}

fn extra_encoding() -> Option<&'static encoding_rs::Encoding> {
    match CODEPAGE.get() {
        Some(opt) => *opt,
        None => Some(encoding_rs::GBK), // 未初始化（测试环境）默认简中 GBK
    }
}

/// 密码候选字节编码：压缩包里的密码只是一串字节，各格式/工具的编码约定不一——
/// - 7z：UTF-16LE（规范定义，SevenzVFS 的 Password::from 已按此转换，不走候选）
/// - RAR5 / 跨平台工具（Python、Linux zip 命令）：UTF-8
/// - Windows 老工具（WinRAR/7-Zip/Bandizip）创建 zip / RAR4 时按系统 ANSI 代码页（配置项，默认 GBK）
/// 故生成 UTF-8 + 配置代码页两个候选，解压时依次尝试；纯 ASCII 时两者相同只留一个。
pub(crate) fn password_candidates(pw: &str) -> Vec<Vec<u8>> {
    let utf8 = pw.as_bytes().to_vec();
    if pw.is_ascii() {
        return vec![utf8];
    }
    let mut out = vec![utf8];
    if let Some(enc) = extra_encoding() {
        let (bytes, _, had_errors) = enc.encode(pw);
        if !had_errors {
            let b = bytes.into_owned();
            if b != out[0] {
                out.push(b);
            }
        }
    }
    out
}

/// 解压临时文件路径：dest.xxx.part{pid}_{seq}（pid+序号）
/// 写临时文件 + 原子改名，避免半截文件被当成有效缓存；
/// pid：两个程序实例共享缓存目录时 dest 相同，pid 区分各自的临时文件；
/// 序号：同进程内同一条目被并发解压时互不踩踏。
/// 注意 cleanup_partial 按 "part" 前缀匹配，可清理 part* 全部形态。
pub(crate) fn tmp_path(dest: &Path) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dest.with_extension(format!("part{}_{}", std::process::id(), n))
}

/// 压缩包内单个条目（扁平列出，含完整路径信息）
#[derive(Debug, Clone)]
pub struct VirtualEntry {
    /// 在压缩包内的完整路径（如 "dir/sub/a.mp4"）
    pub name: String,
    /// 解压后大小（字节）
    pub size: u64,
    /// 是否目录
    pub is_dir: bool,
    /// 条目内容是否加密（文件名仍明文可见；加密头的包在 open 阶段就要密码）
    pub encrypted: bool,
}

/// 可读可 seek 的虚拟文件 —— mpv stream_cb 与（未来的）图片查看器共用。
pub trait VirtualFile: Read + Seek {
    fn size(&self) -> u64;
}

/// 播放策略声明
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// seek 便宜（ZIP / 非固态），流式按需读取
    FastSeek,
    /// 固态，需后台解压回退（Phase 2）
    Solid,
}

#[derive(Debug)]
pub enum VfsError {
    Io(std::io::Error),
    Archive(String),
    Unsupported(String),
    Password,
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsError::Io(e) => write!(f, "IO 错误: {e}"),
            VfsError::Archive(s) => write!(f, "压缩包错误: {s}"),
            VfsError::Unsupported(s) => write!(f, "不支持: {s}"),
            VfsError::Password => write!(f, "需要密码或密码错误"),
        }
    }
}

impl std::error::Error for VfsError {}

impl From<std::io::Error> for VfsError {
    fn from(e: std::io::Error) -> Self {
        VfsError::Io(e)
    }
}

/// 统一三种压缩包格式的读取接口（注册进全局注册表，跨线程共享）
pub trait ArchiveVFS: Send + Sync {
    /// 列出所有 entry
    fn list(&self) -> Vec<VirtualEntry>;
    /// 打开单个文件，返回可读可 seek 的虚拟文件（仅 FastSeek 格式支持）
    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError>;
    /// 决定播放策略
    fn access_mode(&self) -> AccessMode;
    /// 解压单个文件到目标路径（固态格式的慢路径；progress 回调报告已写出字节数）。
    /// 默认不支持；ZipVFS/SevenzVFS/RarVFS 各自实现。
    fn extract_entry(
        &self,
        entry: &VirtualEntry,
        dest: &Path,
        progress: &dyn Fn(u64),
    ) -> Result<(), VfsError> {
        let _ = (entry, dest, progress);
        Err(VfsError::Unsupported("此格式不支持解压".into()))
    }
}

/// 按扩展名打开压缩包（7z/RAR 走固态慢路径，ZIP 走流式快路径）。
/// 加密头（7z/RAR 加密文件名）在打开阶段就会返回 Password，由 UI 弹密码框后带密码重开；
/// 只加密内容（文件名明文）的包 open 成功，播放加密条目时再返回 Password。
pub fn open_archive(
    path: &Path,
    password: Option<&str>,
) -> Result<Box<dyn ArchiveVFS>, VfsError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "zip" => {
            zip_vfs::ZipVFS::open(path, password).map(|v| Box::new(v) as Box<dyn ArchiveVFS>)
        }
        "7z" => {
            sevenz_vfs::SevenzVFS::open(path, password)
                .map(|v| Box::new(v) as Box<dyn ArchiveVFS>)
        }
        "rar" => {
            rar_vfs::RarVFS::open(path, password)
                .map(|v| Box::new(v) as Box<dyn ArchiveVFS>)
        }
        _ => Err(VfsError::Unsupported(format!("暂不支持的格式: .{ext}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 切包场景链路测试：A（加密 7z）→ B（普通 zip）→ A。
    /// 不依赖 GUI，按 App::open_path_pw / start_extract 的真实调用序列模拟：
    /// - register 返回稳定 archive_id：A 切走再切回 id 不变，B 的 id 不同
    /// - dest_path 由 id 派生：切回后缓存路径与首开一致，metadata 命中（免重新解压）
    /// - 密码命中：带记忆密码重开 A 成功（pw_memory 的 UI 逻辑只是 HashMap 查询，
    ///   此处验证的是"命中后"的打开 + 缓存复用全链路）
    #[test]
    fn switch_archive_cache_and_password_rehit() {
        use crate::cache;
        use crate::config::Config;
        use crate::player::stream_cb;
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("switch_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // ---- 造包 A：AES 内容加密的 7z ----
        let payload_a = dir.join("pa.bin");
        std::fs::write(&payload_a, vec![5u8; 60_000]).unwrap();
        let arch_a = dir.join("a.7z");
        {
            use sevenz_rust::Password;
            let mut w = sevenz_rust::SevenZWriter::create(&arch_a).unwrap();
            w.set_encrypt_header(false);
            w.set_content_methods(vec![sevenz_rust::AesEncoderOptions::new(Password::from("pw123")).into()]);
            let e = sevenz_rust::SevenZArchiveEntry::from_path(&payload_a, "enc/pa.bin".into());
            w.push_archive_entry(e, Some(std::fs::File::open(&payload_a).unwrap()))
                .unwrap();
            w.finish().unwrap();
        }
        std::fs::remove_file(&payload_a).unwrap();

        // ---- 造包 B：普通 zip ----
        let arch_b = dir.join("b.zip");
        {
            let file = std::fs::File::create(&arch_b).unwrap();
            let mut w = zip::write::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("b.bin", opts).unwrap();
            w.write_all(&vec![7u8; 4096]).unwrap();
            w.finish().unwrap();
        }

        // ---- 阶段 1：首开 A（无密码）→ 解压触发 Password（UI 弹密码框的路径）----
        let vfs = open_archive(&arch_a, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        let cfg = Config::default();
        let cdir = cache::cache_dir(&cfg);
        assert!(
            matches!(vfs.extract_entry(&entries[0], &cache::dest_path(&cdir, 0, 0, &entries[0].name), &|_| {}), Err(VfsError::Password)),
            "无密码解压应报 Password"
        );
        let (id_a1, _) = stream_cb::register(std::sync::Arc::from(vfs), &arch_a);
        let dest_a = cache::dest_path(&cdir, id_a1, 0, &entries[0].name);

        // ---- 阶段 2：用户输入密码后带密码重开 A，解压成功（缓存落盘）----
        let arc_a2: std::sync::Arc<dyn ArchiveVFS> =
            open_archive(&arch_a, Some("pw123")).unwrap().into();
        let (id_a2, entries2) = stream_cb::register(arc_a2.clone(), &arch_a);
        assert_eq!(id_a1, id_a2, "同一路径重开 register 应返回相同稳定 id");
        arc_a2.extract_entry(&entries2[0], &dest_a, &|_| {}).unwrap();
        assert_eq!(
            std::fs::metadata(&dest_a).unwrap().len(),
            entries2[0].size,
            "解压后缓存应存在且尺寸一致"
        );

        // ---- 阶段 3：切到 B ----
        let vfs_b = open_archive(&arch_b, None).unwrap();
        let (id_b, entries_b) = stream_cb::register(
            std::sync::Arc::from(vfs_b),
            &arch_b,
        );
        assert_ne!(id_a1, id_b, "不同包的 id 不应相同");
        assert_eq!(entries_b.len(), 1);

        // ---- 阶段 4：切回 A（模拟 pw_memory 命中：直接带记忆密码重开）----
        let vfs = open_archive(&arch_a, Some("pw123")).unwrap(); // 密码命中：不弹框直接打开
        let (id_a3, entries3) = stream_cb::register(
            std::sync::Arc::from(vfs),
            &arch_a,
        );
        assert_eq!(id_a1, id_a3, "切回 A 时 id 应保持稳定");
        // 缓存命中判定（App::start_extract 的同款逻辑）
        let dest_a3 = cache::dest_path(&cdir, id_a3, 0, &entries3[0].name);
        assert_eq!(dest_a3, dest_a, "切回后缓存路径应与首开一致");
        assert!(
            std::fs::metadata(&dest_a3)
                .map(|m| m.len() == entries3[0].size)
                .unwrap_or(false),
            "切回 A 缓存应命中（存在且尺寸一致），无需重新解压"
        );

        // 记忆密码失效兜底：包被换密码时错误密码解压报 Password → UI 重新弹框
        let arc_bad: std::sync::Arc<dyn ArchiveVFS> =
            open_archive(&arch_a, Some("wrongpw")).unwrap().into();
        assert!(matches!(
            arc_bad.extract_entry(&entries3[0], &dest_a, &|_| {}),
            Err(VfsError::Password)
        ));

        // 环境清理：只删本测试的缓存文件与测试目录，不动共享缓存目录其它内容
        std::fs::remove_file(&dest_a).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
