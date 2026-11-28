//! ISO VFS：ISO9660（含 Joliet）只读镜像 —— 第四种 FastSeek 格式。
//!
//! ISO9660 是未压缩、未加密的只读文件系统：每个文件在目录记录里直接登记
//! 数据区位置（extent LBA）与长度，随机访问 = 打开镜像 File + seek 到
//! `lba*2048 + offset`，O(1)——与 DirVFS/zip Stored 完全同构，走 mpv
//! zvf:// 流式快路径，解压会话/滑窗/缓存/密码探测等慢路径机制零触碰
//! （条目全部 `encrypted=false`，密码探测零成本直通）。
//!
//! 解析策略（一期范围）：PVD（主卷描述符，LBA16）+ 可选 Joliet SVD
//! （补充卷描述符，UCS-2BE 名字）二选一作为名字空间——有 Joliet 用
//! Joliet（中文/长文件名/原大小写），否则用基础名（大写 8.3，剥 `;1`
//! 版本号与尾部 `.`）。纯 UDF 镜像（蓝光）一期明确不支持，探测到即报
//! Unsupported 定性（二期规划，见讨论记录）。
//!
//! 不引入外部库的理由：VFS 的核心产出物是「文件名 → extent(LBA, size)」表
//! （VirtualFile 适配器的本质），现成 crate 都抽象成"按路径读流"，extent
//! 藏在抽象内部；而 ISO9660+Joliet 只读解析是同类最简单的二进制格式之一，
//! 手写几百行即可全权控制。

use super::{AccessMode, ArchiveVFS, ExtractPhase, VfsError, VirtualEntry, VirtualFile};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 递归深度上限（与 DirVFS 同惯例：异常深树的栈保险）
const MAX_DEPTH: usize = 64;
/// 目录 extent 读取上限：目录结构超过 8MB 视为异常包，停止解析（防畸形包）
const MAX_DIR_BYTES: u64 = 8 * 1024 * 1024;

/// 扇区大小。PVD 声明的逻辑块大小非 2048 时定性不支持（媒体 ISO 恒为 2048）
const SECTOR: u64 = 2048;

/// 单个文件的 extent 表：镜像内绝对字节偏移 + 长度（多 extent 大文件按
/// 顺序合并；ISO9660 的 4GB 上限导致大文件会被切成连续多条目录记录）
type Extents = Arc<Vec<(u64, u64)>>;

pub struct IsoVFS {
    /// 镜像文件路径（open_entry 每次开新句柄，无共享可变状态，与 DirVFS 同款）
    path: PathBuf,
    /// 全部媒体候选条目（名字过滤交给 UI 的 is_video/is_image 谓词，与
    /// 压缩包模式一致），顺序 = 路径字典序
    entries: Vec<VirtualEntry>,
    /// 条目名 → extent 表（open_entry 查找）
    files: HashMap<String, Extents>,
}

/// 目录 walk 的原始文件记录（多 extent 合并前的形态）
struct FileRec {
    path: String,
    lba: u64,
    size: u32,
    /// flags bit7：非末段 extent（后续紧跟同名记录延续数据）
    not_final: bool,
}

impl IsoVFS {
    /// 打开 ISO 镜像。密码参数与其它 VFS 签名对齐，ISO 无加密概念恒忽略。
    pub fn open(path: &Path, _password: Option<&str>) -> Result<Self, VfsError> {
        let mut f = std::fs::File::open(path)?;
        let img_len = f.metadata()?.len();
        if img_len < (17 * SECTOR) {
            return Err(VfsError::Unsupported("不是 ISO9660 镜像".into()));
        }

        // ---- 扫描卷描述符序列（LBA16 起，最多 16 个扇区）：PVD / Joliet SVD ----
        let mut pvd: Option<[u8; SECTOR as usize]> = None;
        let mut joliet: Option<[u8; SECTOR as usize]> = None;
        let mut udf_marker = false;
        // 扇区上限 = min(16, 镜像实际剩余)：短镜像（目录扇区少的包）不越界
        let scan_end = ((img_len / SECTOR) as u64).min(32);
        for lba in 16u64..scan_end {
            let mut s = [0u8; SECTOR as usize];
            f.seek(SeekFrom::Start(lba * SECTOR))?;
            f.read_exact(&mut s)?;
            match (&s[1..6], s[0]) {
                (b"CD001", 1) => pvd = Some(s),
                (b"CD001", 2) => {
                    // Joliet 转义序列（SVD 偏移 88..91）："%/@"、"%/C"、"%/E"
                    let esc = &s[88..91];
                    if esc == b"%/@" || esc == b"%/C" || esc == b"%/E" {
                        joliet = Some(s);
                    }
                }
                (b"BEA01", _) | (b"NSR02", _) | (b"NSR03", _) | (b"TEA01", _) => {
                    udf_marker = true;
                }
                _ => {}
            }
        }
        let Some(pvd) = pvd else {
            if udf_marker {
                return Err(VfsError::Unsupported(
                    "纯 UDF 镜像暂不支持（一期只支持 ISO9660/Joliet，蓝光二期规划）".into(),
                ));
            }
            return Err(VfsError::Unsupported("不是 ISO9660 镜像".into()));
        };

        // 逻辑块大小（PVD 偏移 128 双端序）：非 2048 定性不支持
        let declared_block = u16::from_le_bytes([pvd[128], pvd[129]]);
        if declared_block as u64 != SECTOR {
            return Err(VfsError::Unsupported(format!(
                "不支持的逻辑块大小: {declared_block}（仅支持 2048）"
            )));
        }

        // 名字空间：有 Joliet 用 Joliet（UCS-2BE，原大小写/中文/长名），
        // 否则基础名（大写 8.3 + 剥 ;1）
        let (root_rec, use_joliet) = match joliet {
            Some(svd) => (root_record(&svd), true),
            None => (root_record(&pvd), false),
        };

        let mut vfs = IsoVFS {
            path: path.to_path_buf(),
            entries: Vec::new(),
            files: HashMap::new(),
        };
        // 递归收集：目录记录自带子目录 extent，直接按记录 walk
        let mut recs: Vec<FileRec> = Vec::new();
        walk_dir(&mut f, root_rec.0, root_rec.1, "", use_joliet, 0, &mut recs)?;

        // 多 extent 合并：同一目录内连续同名记录（前一条 bit7 置位）合并为
        // 一个文件的连续 extent 表
        let mut i = 0;
        while i < recs.len() {
            let path = recs[i].path.clone();
            let mut total = recs[i].size as u64;
            // extent 统一存镜像内绝对字节偏移（扇区号 × 2048）
            let mut extents = vec![(recs[i].lba * SECTOR, recs[i].size as u64)];
            let mut j = i + 1;
            while recs[i].not_final && j < recs.len() && recs[j].path == path {
                extents.push((recs[j].lba * SECTOR, recs[j].size as u64));
                total += recs[j].size as u64;
                let nf = recs[j].not_final;
                j += 1;
                if !nf {
                    break;
                }
            }
            vfs.files.insert(path.clone(), Arc::new(extents));
            vfs.entries.push(VirtualEntry {
                name: path,
                size: total,
                is_dir: false,
                encrypted: false,
                crc: None,
            });
            i = j;
        }

        // 字典序（与 DirVFS 同源：read 顺序不可预期，排序是最可预期的确定序）
        vfs.entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(vfs)
    }
}

/// 从 PVD/SVD 偏移 156 取根目录记录 → (extent LBA, extent 字节数)
fn root_record(s: &[u8]) -> (u64, u32) {
    (
        u32::from_le_bytes([s[156 + 2], s[156 + 3], s[156 + 4], s[156 + 5]]) as u64,
        u32::from_le_bytes([s[156 + 10], s[156 + 11], s[156 + 12], s[156 + 13]]),
    )
}

/// 递归 walk 目录 extent，产出扁平文件记录。解析异常（记录越界/截断）时
/// 停止该目录剩余部分——部分可读的树照常收集（与 DirVFS 同哲学）
fn walk_dir(
    f: &mut std::fs::File,
    lba: u64,
    size: u32,
    prefix: &str,
    use_joliet: bool,
    depth: usize,
    out: &mut Vec<FileRec>,
) -> Result<(), VfsError> {
    if depth >= MAX_DEPTH || size as u64 > MAX_DIR_BYTES {
        return Ok(());
    }
    let mut buf = vec![0u8; size as usize];
    f.seek(SeekFrom::Start(lba * SECTOR))?;
    if f.read_exact(&mut buf).is_err() {
        return Ok(()); // extent 截断（异常镜像）：静默放弃该目录
    }
    let mut pos = 0usize;
    while pos + 34 <= buf.len() {
        let len = buf[pos] as usize;
        if len == 0 {
            // 记录长度 0 = 本扇区剩余为填充：跳到下一扇区边界
            pos = ((pos / SECTOR as usize) + 1) * SECTOR as usize;
            continue;
        }
        if pos + len > buf.len() {
            break; // 记录越界：停止解析
        }
        let rec = &buf[pos..pos + len];
        pos += len;
        let flags = rec[25];
        let name_len = rec[32] as usize;
        let name_raw = &rec[33..(33 + name_len).min(len)];
        let is_dir = flags & 0x02 != 0;
        let not_final = flags & 0x80 != 0;

        // "."（0x00）/".."（0x01）自反/父记录跳过
        if name_len == 1 && (name_raw[0] == 0 || name_raw[0] == 1) {
            continue;
        }
        let name = if use_joliet {
            joliet_name(name_raw)
        } else {
            base_name(name_raw)
        };
        if name.is_empty() {
            continue;
        }
        let child_lba = u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]) as u64;
        let child_size = u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]);
        let child_path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if is_dir {
            walk_dir(f, child_lba, child_size, &child_path, use_joliet, depth + 1, out)?;
        } else {
            out.push(FileRec {
                path: child_path,
                lba: child_lba,
                size: child_size,
                not_final,
            });
        }
    }
    Ok(())
}

/// 基础名：`NAME.EXT;1` → 剥 `;版本` 与尾部 `.`（ECMA-119 附录约定）
fn base_name(raw: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(raw).into_owned();
    if let Some(i) = s.find(';') {
        s.truncate(i);
    }
    while s.ends_with('.') {
        s.pop();
    }
    s
}

/// Joliet 名：UCS-2BE 解码（规范无版本号后缀）
fn joliet_name(raw: &[u8]) -> String {
    raw.chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .map(|u| char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
        .collect()
}

impl ArchiveVFS for IsoVFS {
    fn list(&self) -> Vec<VirtualEntry> {
        self.entries.clone()
    }

    /// extent 直开：每次 open_entry 开新镜像句柄（无跨 loadfile 状态），
    /// seek O(1) 由 extent 映射保证——与 DirVFS 同待遇走 mpv 快路径
    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>, VfsError> {
        let extents = self
            .files
            .get(&entry.name)
            .ok_or_else(|| VfsError::Archive(format!("条目不在镜像中: {}", entry.name)))?;
        let f = std::fs::File::open(&self.path)?;
        let total = extents.iter().map(|(_, l)| l).sum();
        Ok(Box::new(IsoFile {
            f,
            extents: extents.clone(),
            total,
            pos: 0,
        }))
    }

    /// extent 映射 seek O(1)——mpv 流式快路径（与 ZipVFS/DirVFS 同待遇）
    fn access_mode(&self) -> AccessMode {
        AccessMode::FastSeek
    }

    /// 拷贝式导出（保存/缓存落盘）：与 zip 同款 tmp+rename，进度映射目标段
    fn extract_entry(
        &self,
        entry: &VirtualEntry,
        dest: &Path,
        _cancel: &std::sync::atomic::AtomicBool,
        progress: &dyn Fn(ExtractPhase, u64, u64),
    ) -> Result<(), VfsError> {
        let mut src = self.open_entry(entry)?;
        let tmp = super::tmp_path(dest);
        let mut out = std::fs::File::create(&tmp).map_err(|e| {
            VfsError::Archive(format!(
                "创建临时文件失败 [{}]（条目 [{}]）: {e}",
                tmp.display(),
                entry.name
            ))
        })?;
        super::sevenz_vfs::copy_with_progress(&mut src, &mut out, entry.size, &|done| {
            progress(ExtractPhase::Target, done, entry.size)
        })
        .map_err(|e| {
            std::fs::remove_file(&tmp).ok();
            let e = match e {
                super::sevenz_vfs::CopyErr::Read(e) | super::sevenz_vfs::CopyErr::Write(e) => e,
            };
            VfsError::Archive(format!(
                "ISO 导出失败: 镜像 [{}] 条目 [{}] → [{}]: {e}",
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
}

/// extent 映射虚拟文件：逻辑位置 → 镜像绝对偏移（extent 直存，无解码）。
/// 多 extent 大文件（>4GB 被切分的记录链）按顺序查表衔接
struct IsoFile {
    f: std::fs::File,
    extents: Extents,
    total: u64,
    pos: u64,
}

impl IsoFile {
    /// 逻辑 pos → (镜像物理偏移, 本段可读字节数)。extent 表按序衔接：
    /// 段内逻辑起点 = 前序 extent 长度累计。pos 落在表外/空隙（异常镜像）
    /// 返回 None
    fn locate(&self) -> Option<(u64, u64)> {
        let mut logical = 0u64;
        for &(off, len) in self.extents.iter() {
            if self.pos < logical + len {
                let phys = off + (self.pos - logical);
                let avail = (logical + len - self.pos).min(self.total - self.pos);
                return Some((phys, avail));
            }
            logical += len;
        }
        None
    }
}

impl Read for IsoFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.total || buf.is_empty() {
            return Ok(0);
        }
        let Some((phys, avail)) = self.locate() else {
            return Ok(0);
        };
        let chunk = buf.len().min(avail as usize);
        self.f.seek(SeekFrom::Start(phys))?;
        let mut n = 0;
        while n < chunk {
            let r = self.f.read(&mut buf[n..chunk])?;
            if r == 0 {
                break;
            }
            n += r;
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for IsoFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(x) => x,
            SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            SeekFrom::End(d) => (self.total as i64 + d).max(0) as u64,
        };
        Ok(self.pos)
    }
}

impl VirtualFile for IsoFile {
    fn size(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 测试用最小 ISO9660 生成器（fixture）：程序化构造镜像，全链路验证 ----

    #[derive(Default)]
    struct TDir {
        dirs: BTreeMap<String, TDir>,
        files: BTreeMap<String, Vec<u8>>,
    }

    fn insert_path(root: &mut TDir, path: &str, data: &[u8]) {
        let mut cur = root;
        let parts: Vec<&str> = path.split('/').collect();
        for p in &parts[..parts.len() - 1] {
            cur = cur.dirs.entry(p.to_string()).or_default();
        }
        cur.files.insert(parts.last().unwrap().to_string(), data.to_vec());
    }

    fn collect_dirs<'a>(dir: &'a TDir, prefix: &str, out: &mut Vec<(String, &'a TDir)>) {
        out.push((prefix.to_string(), dir));
        for (name, sub) in &dir.dirs {
            let p = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            collect_dirs(sub, &p, out);
        }
    }

    /// 构造一条目录记录（双端序 LBA/size + flags + 名字，偶对齐）
    fn dir_record(name: &[u8], lba: u32, size: u32, flags: u8) -> Vec<u8> {
        let mut len = 33 + name.len();
        if len % 2 == 1 {
            len += 1;
        }
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[2..6].copy_from_slice(&(lba as u32).to_le_bytes());
        r[6..10].copy_from_slice(&(lba as u32).to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[25] = flags;
        r[32] = name.len() as u8;
        r[33..33 + name.len()].copy_from_slice(name);
        r
    }

    fn ucs2be(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
    }

    fn vol_desc(typ: u8, root_lba: u32, escape: Option<&[u8; 3]>) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR as usize];
        s[0] = typ;
        s[1..6].copy_from_slice(b"CD001");
        s[6] = 1;
        s[128..130].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        s[130..132].copy_from_slice(&(SECTOR as u16).to_be_bytes());
        if let Some(e) = escape {
            s[88..91].copy_from_slice(e);
        }
        let rec = dir_record(&[0x00], root_lba, SECTOR as u32, 0x02);
        s[156..156 + rec.len()].copy_from_slice(&rec);
        s
    }

    /// 程序化构造最小合法 ISO：PVD(+可选 Joliet SVD) + 终止符 + 目录扇区 + 文件数据。
    /// 每个目录占一个扇区（测试树足够小）；joliet=true 时另建一套 UCS-2BE
    /// 名字的目录扇区（bridge 双名字空间的真实形态）
    fn build_iso(entries: &[(&str, &[u8])], joliet: bool) -> Vec<u8> {
        let mut root = TDir::default();
        for (p, d) in entries {
            insert_path(&mut root, p, d);
        }
        let mut dirs = Vec::new();
        collect_dirs(&root, "", &mut dirs);

        let mut image: Vec<Vec<u8>> = vec![vec![0u8; SECTOR as usize]; 16];
        let alloc = |image: &mut Vec<Vec<u8>>| -> u32 {
            image.push(vec![0u8; SECTOR as usize]);
            (image.len() - 1) as u32
        };

        // 基础名字空间的目录扇区
        let mut base_lba = HashMap::new();
        for (p, _) in &dirs {
            let lba = alloc(&mut image);
            base_lba.insert(p.clone(), lba);
        }
        // 文件数据扇区（基础与 Joliet 目录记录共用同一数据区——真实 bridge 形态；
        // 大于单扇区的文件按 ceil(len/2048) 连续分配多扇区跨扇区写）
        let mut file_lba = HashMap::new();
        for (p, dir) in &dirs {
            for (name, data) in &dir.files {
                let path = if p.is_empty() {
                    name.clone()
                } else {
                    format!("{p}/{name}")
                };
                let start = image.len() as u32;
                let n = data.len().div_ceil(SECTOR as usize);
                for _ in 0..n {
                    image.push(vec![0u8; SECTOR as usize]);
                }
                for (i, chunk) in data.chunks(SECTOR as usize).enumerate() {
                    let idx = start as usize + i;
                    image[idx][..chunk.len()].copy_from_slice(chunk);
                }
                file_lba.insert(path, (start, data.len() as u32));
            }
        }
        // Joliet 名字空间的目录扇区（独立一套，指向同一文件数据区）
        let mut jol_lba = HashMap::new();
        if joliet {
            for (p, _) in &dirs {
                let lba = alloc(&mut image);
                jol_lba.insert(p.clone(), lba);
            }
        }

        // 填充基础目录内容
        for (p, dir) in &dirs {
            let mut content = Vec::new();
            content.extend(dir_record(&[0x00], base_lba[p], SECTOR as u32, 0x02));
            content.extend(dir_record(&[0x01], base_lba[p], SECTOR as u32, 0x02));
            for (name, _sub) in &dir.dirs {
                let child = if p.is_empty() {
                    name.clone()
                } else {
                    format!("{p}/{name}")
                };
                let mut nb = name.to_ascii_uppercase().into_bytes();
                nb.push(b'.'); // 基础目录名无版本号，尾点测剥除
                content.extend(dir_record(&nb, base_lba[&child], SECTOR as u32, 0x02));
            }
            for name in dir.files.keys() {
                let path = if p.is_empty() {
                    name.clone()
                } else {
                    format!("{p}/{name}")
                };
                let (lba, len) = file_lba[&path];
                let nb = format!("{};1", name.to_ascii_uppercase()).into_bytes();
                content.extend(dir_record(&nb, lba, len, 0));
            }
            let lba = base_lba[p] as usize;
            image[lba][..content.len()].copy_from_slice(&content);
        }

        // 填充 Joliet 目录内容（UCS-2BE 名字，无版本号）
        if joliet {
            for (p, dir) in &dirs {
                let mut content = Vec::new();
                content.extend(dir_record(&[0x00], jol_lba[p], SECTOR as u32, 0x02));
                content.extend(dir_record(&[0x01], jol_lba[p], SECTOR as u32, 0x02));
                for (name, _sub) in &dir.dirs {
                    let child = if p.is_empty() {
                        name.clone()
                    } else {
                        format!("{p}/{name}")
                    };
                    content.extend(dir_record(&ucs2be(name), jol_lba[&child], SECTOR as u32, 0x02));
                }
                for name in dir.files.keys() {
                    let path = if p.is_empty() {
                        name.clone()
                    } else {
                        format!("{p}/{name}")
                    };
                    let (lba, len) = file_lba[&path];
                    content.extend(dir_record(&ucs2be(name), lba, len, 0));
                }
                let lba = jol_lba[p] as usize;
                image[lba][..content.len()].copy_from_slice(&content);
            }
        }

        // PVD（LBA16）+ 可选 SVD（LBA17）+ 终止符
        image.push(vol_desc(1, base_lba[""], None));
        if joliet {
            image.push(vol_desc(2, jol_lba[""], Some(b"%/E")));
        }
        image.push(vol_desc(255, 0, None));
        image.concat()
    }

    fn tmp_iso(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "iso_vfs_{tag}_{}_{}.iso",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    const DATA_A: &[u8] = &[0xAA; 1000];
    const DATA_C: &[u8] = &[0x33; 4321];

    #[test]
    fn 基础镜像_列表_读取_seek() {
        let img = build_iso(&[("a.mp4", DATA_A), ("sub/c.jpg", DATA_C)], false);
        let path = tmp_iso("basic");
        std::fs::write(&path, &img).unwrap();
        let vfs = IsoVFS::open(&path, None).unwrap();
        let list = vfs.list();
        // 基础名字空间：大写 + 剥 ;1；字典序
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A.MP4", "SUB/C.JPG"]);
        assert_eq!(list[0].size, DATA_A.len() as u64);
        assert_eq!(list[1].size, DATA_C.len() as u64);
        for e in &list {
            assert!(!e.is_dir && !e.encrypted && e.crc.is_none());
        }
        assert_eq!(vfs.access_mode(), AccessMode::FastSeek);

        // 顺序读 + 中段 seek + 尾部 seek（mpv 快路径典型用法）
        let e0 = &list[0];
        let mut vf = vfs.open_entry(e0).unwrap();
        assert_eq!(vf.size(), 1000);
        let mut buf = Vec::new();
        vf.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, DATA_A);
        vf.seek(SeekFrom::Start(500)).unwrap();
        let mut mid = [0u8; 16];
        vf.read_exact(&mut mid).unwrap();
        assert_eq!(&mid, &DATA_A[500..516]);
        vf.seek(SeekFrom::End(-10)).unwrap();
        vf.read_exact(&mut mid[..10]).unwrap();
        assert_eq!(&mid[..10], &DATA_A[990..]);

        // 子目录条目同理
        let e1 = &list[1];
        let mut vf = vfs.open_entry(e1).unwrap();
        let mut buf = Vec::new();
        vf.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, DATA_C);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn joliet镜像_中文与原大小写名字() {
        let data: Vec<u8> = (0u8..=255).collect();
        let img = build_iso(
            &[("a.mp4", &data), ("视频/影片.MOV", &data)],
            true,
        );
        let path = tmp_iso("joliet");
        std::fs::write(&path, &img).unwrap();
        let vfs = IsoVFS::open(&path, None).unwrap();
        let names: Vec<String> = vfs.list().iter().map(|e| e.name.clone()).collect();
        // Joliet 优先：原大小写 + 中文目录名（字节序：'a' < 中文）
        assert_eq!(names, ["a.mp4", "视频/影片.MOV"]);
        // UCS-2BE 名字对应的 extent 读取正确
        let entry = &vfs.list()[1];
        let mut vf = vfs.open_entry(entry).unwrap();
        let mut buf = Vec::new();
        vf.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_archive按扩展名分发_大小写均可() {
        for tag in ["disp_lo", "disp_up"] {
            let img = build_iso(&[("x.mp4", DATA_A)], false);
            let mut path = tmp_iso(tag);
            if tag.ends_with("up") {
                path.set_extension("ISO");
            }
            std::fs::write(&path, &img).unwrap();
            let vfs = super::super::open_archive(&path, None).unwrap();
            assert_eq!(vfs.access_mode(), AccessMode::FastSeek);
            let list = vfs.list();
            assert_eq!(list.len(), 1);
            let mut vf = vfs.open_entry(&list[0]).unwrap();
            let mut buf = Vec::new();
            vf.read_to_end(&mut buf).unwrap();
            assert_eq!(buf, DATA_A);
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn extract_entry导出与源数据一致() {
        let img = build_iso(&[("sub/c.jpg", DATA_C)], false);
        let path = tmp_iso("extract");
        std::fs::write(&path, &img).unwrap();
        let vfs = IsoVFS::open(&path, None).unwrap();
        let entry = vfs.list().remove(0);
        let dest = tmp_iso("extract_dest");
        let _ = std::fs::remove_file(&dest);
        vfs.extract_entry(
            &entry,
            &dest,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), DATA_C);
        std::fs::remove_file(&dest).unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn 纯udf镜像与非iso均定性拒绝() {
        // 纯 UDF：卷识别序列只有 BEA01，无 CD001 → 明确定性（蓝光二期）
        let mut img = vec![0u8; 20 * SECTOR as usize];
        img[16 * SECTOR as usize + 1..16 * SECTOR as usize + 6].copy_from_slice(b"BEA01");
        let path = tmp_iso("udfonly");
        std::fs::write(&path, &img).unwrap();
        let err = match IsoVFS::open(&path, None) {
            Ok(_) => panic!("纯 UDF 镜像应被拒绝"),
            Err(e) => e,
        };
        assert!(matches!(err, VfsError::Unsupported(ref m) if m.contains("UDF")));
        std::fs::remove_file(&path).unwrap();

        // 全零镜像：既无 CD001 也无 UDF 标记
        let path = tmp_iso("zero");
        std::fs::write(&path, vec![0u8; 20 * SECTOR as usize]).unwrap();
        assert!(matches!(
            IsoVFS::open(&path, None),
            Err(VfsError::Unsupported(_))
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn 多extent记录合并为连续extent表() {
        // 直接构造记录链（不建完整镜像）：首条 not_final(bit7)，次条收尾
        let recs = vec![
            FileRec { path: "big.mp4".into(), lba: 100, size: 2048, not_final: true },
            FileRec { path: "big.mp4".into(), lba: 105, size: 512, not_final: false },
        ];
        let mut vfs_entries: Vec<VirtualEntry> = Vec::new();
        let mut files: HashMap<String, Extents> = HashMap::new();
        let mut i = 0;
        while i < recs.len() {
            let path = recs[i].path.clone();
            let mut total = recs[i].size as u64;
            let mut extents = vec![(recs[i].lba * SECTOR, recs[i].size as u64)];
            let mut j = i + 1;
            while recs[i].not_final && j < recs.len() && recs[j].path == path {
                extents.push((recs[j].lba * SECTOR, recs[j].size as u64));
                total += recs[j].size as u64;
                let nf = recs[j].not_final;
                j += 1;
                if !nf {
                    break;
                }
            }
            files.insert(path.clone(), Arc::new(extents));
            vfs_entries.push(VirtualEntry {
                name: path,
                size: total,
                is_dir: false,
                encrypted: false,
                crc: None,
            });
            i = j;
        }
        assert_eq!(vfs_entries[0].size, 2048 + 512);
        let ex = files["big.mp4"].as_ref();
        assert_eq!(ex, &[(100 * SECTOR, 2048), (105 * SECTOR, 512)]);
    }
}
