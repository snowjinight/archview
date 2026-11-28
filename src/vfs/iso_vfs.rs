//! ISO VFS：ISO9660（含 Joliet）+ 纯 UDF 2.50（蓝光）只读镜像 —— FastSeek 快路径。
//!
//! 两种形态同一条数据通道：解析产出「文件名 → extent(镜像绝对字节偏移, 长度)」表，
//! `IsoFile` 适配器做逻辑位置 → 物理偏移映射，O(1) seek——与 DirVFS/zip Stored
//! 同构，走 mpv zvf:// 流式快路径，解压会话/滑窗/缓存/密码探测等慢路径机制
//! 零触碰（条目全部 `encrypted=false`，密码探测零成本直通）。
//!
//! **ISO9660 层**：PVD（LBA16）+ 可选 Joliet SVD（UCS-2BE 名字）二选一作为
//! 名字空间——有 Joliet 用 Joliet（中文/长文件名/原大小写），否则基础名
//! （大写 8.3，剥 `;1` 版本号与尾部 `.`）。bridge 双结构盘走 ISO 侧。
//!
//! **UDF 2.50 层**（纯 UDF 蓝光镜像，BDMV）：无 CD001 主卷时改走 UDF 指针链
//! AVDP(256) → VDS(PD+LVD) → 元数据分区 → FSD → FID 树。分区语义：
//! ICB/FID/目录数据在元数据分区，**文件数据 short_ad 永远落在物理分区**
//! （元数据分区只装结构）；换算绝对偏移后与 ISO9660 共用 extent 通道。
//! 不引入外部库的理由：VFS 的核心产出物是 extent 表（VirtualFile 适配器的
//! 本质），现成 crate（hadris-udf 拒绝 metadata partition / hivepack::iso
//! 顺序流式）各有结构性不贴合；而只读解析手写可全权控制（UDF 布局参照
//! libfreemkv/udf.rs——针对 BD-ROM 实测过的指针链）。

use super::{AccessMode, ArchiveVFS, ExtractPhase, VfsError, VirtualEntry, VirtualFile};
use std::collections::HashMap;
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
            // 无 ISO9660 主卷：纯 UDF（蓝光镜像）改走 UDF 指针链
            if img_len >= (UDF_AVDP_LBA + 1) * SECTOR {
                if let Some(avdp) = read_sector(&mut f, UDF_AVDP_LBA) {
                    if u16::from_le_bytes([avdp[0], avdp[1]]) == 2 {
                        return udf_open(&mut f, path);
                    }
                }
            }
            if udf_marker {
                return Err(VfsError::Unsupported(
                    "UDF 镜像结构异常（AVDP 缺失或损坏）".into(),
                ));
            }
            return Err(VfsError::Unsupported("不是 ISO9660/UDF 镜像".into()));
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

// ==================== UDF 2.50（纯 UDF 蓝光镜像） ====================
//
// 指针链（参照 libfreemkv/udf.rs——对真 BD-ROM 实测过的布局）：
//   AVDP(扇区256, tag 2) → VDS(32..64: PVD tag1 / PD tag5 / LVD tag6 / TD tag8)
//   → 元数据文件 FE（物理分区，lbn 由 LVD 的 type2 分区映射声明）
//   → 元数据分区内容 → FSD(tag 256) → 根目录 ICB → FID(tag 257) 树
// 分区语义：ICB/FID/目录数据在元数据分区；文件数据 short_ad 永远落在物理
// 分区。统一换算成镜像绝对字节偏移后进与 ISO9660 相同的 extent 通道。

const UDF_AVDP_LBA: u64 = 256;
const UDF_VDS_SCAN_START: u64 = 32;
const UDF_VDS_SCAN_END: u64 = 64;
/// 目录单段上限同 ISO 侧；AD 连续链最多跟进 8 次（防异常盘死循环）
const UDF_AD_CHAIN_MAX: usize = 8;

fn read_sector(f: &mut std::fs::File, lba: u64) -> Option<[u8; 2048]> {
    let mut s = [0u8; 2048];
    f.seek(SeekFrom::Start(lba * SECTOR)).ok()?;
    f.read_exact(&mut s).ok()?;
    Some(s)
}

struct UdfVol {
    /// 物理分区起始（绝对扇区）：文件数据 extent 的基准
    partition_start: u64,
    /// 元数据分区内容起始（绝对扇区）：ICB/FID/目录数据的基准
    metadata_start: u64,
}

struct UdfFileRec {
    path: String,
    /// info_length（FE/EFE 偏移 56 的 u64，FE/EFE 同位）
    size: u64,
    extents: Vec<(u64, u64)>,
}

/// UDF 卷打开：VDS → 元数据分区 → FSD → 全树 walk → 复用 ISO 的 extent 通道
fn udf_open(f: &mut std::fs::File, path: &Path) -> Result<IsoVFS, VfsError> {
    // VDS 扫描（BD-ROM 惯例在 32..64）
    let mut partition_start = 0u64;
    let mut num_maps = 0u32;
    let mut lvd: Option<[u8; 2048]> = None;
    for lba in UDF_VDS_SCAN_START..UDF_VDS_SCAN_END {
        let Some(d) = read_sector(f, lba) else { break };
        match u16::from_le_bytes([d[0], d[1]]) {
            5 => partition_start = u32::from_le_bytes([d[188], d[189], d[190], d[191]]) as u64,
            6 => {
                num_maps = u32::from_le_bytes([d[268], d[269], d[270], d[271]]);
                lvd = Some(d);
            }
            8 => break,
            _ => {}
        }
    }
    if partition_start == 0 || lvd.is_none() {
        return Err(VfsError::Unsupported(
            "UDF 镜像结构异常（VDS 不完整：缺 Partition/Logical Volume Descriptor）".into(),
        ));
    }

    // 元数据分区定位：num_maps ≥ 2 时第二个 map 为 type 2（元数据分区映射，
    // 长 64，实测 mkudffs/BD 盘布局）：4..36 = 类型标识 EntityID（flags +
    // "*UDF Metadata Partition" + 0x0250 修订后缀），40..44 = Metadata File
    // Location（物理分区相对 u32），44..48 = 镜像位置，48..52 = 位图位置
    // （-1 = 无）。注意：位置字段为 0 且 FE 真在物理分区 lba 0 的盘也存在
    // （libfreemkv 硬编码形态）——以 map 字段为准，为 0 时退化试 lba 0
    let mut metadata_start = partition_start;
    if num_maps >= 2 {
        if let Some(lvd) = &lvd {
            let pm1_len = lvd[441] as usize;
            let map2 = 440 + pm1_len;
            if lvd.get(map2) == Some(&2) && map2 + 44 <= 2048 {
                let meta_lbn = u32::from_le_bytes([
                    lvd[map2 + 40],
                    lvd[map2 + 41],
                    lvd[map2 + 42],
                    lvd[map2 + 43],
                ]) as u64;
                for lbn in [meta_lbn, 0] {
                    if let Some(fe) = read_sector(f, partition_start + lbn) {
                        let tag = u16::from_le_bytes([fe[0], fe[1]]);
                        if let Some((ad_off, l_ad)) = udf_ad_area(&fe, tag) {
                            if l_ad >= 8 {
                                let raw = u32::from_le_bytes([
                                    fe[ad_off],
                                    fe[ad_off + 1],
                                    fe[ad_off + 2],
                                    fe[ad_off + 3],
                                ]);
                                let pos = u32::from_le_bytes([
                                    fe[ad_off + 4],
                                    fe[ad_off + 5],
                                    fe[ad_off + 6],
                                    fe[ad_off + 7],
                                ]);
                                if raw >> 30 == 0 {
                                    // 元数据文件 FE 在物理分区，其数据 extent 即元数据内容
                                    metadata_start = partition_start + pos as u64;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let vol = UdfVol { partition_start, metadata_start };

    // FSD（元数据分区 lba 0）→ 根目录 ICB（long_ad 偏移 400，lbn 在 404..408）
    let Some(fsd) = read_sector(f, metadata_start) else {
        return Err(VfsError::Unsupported("UDF 镜像结构异常（FSD 不可读）".into()));
    };
    if u16::from_le_bytes([fsd[0], fsd[1]]) != 256 {
        return Err(VfsError::Unsupported(format!(
            "UDF 镜像结构异常（FSD 缺失 @metadata_start={metadata_start}，partition_start={partition_start}）"
        )));
    }
    let root_lba = u32::from_le_bytes([fsd[404], fsd[405], fsd[406], fsd[407]]) as u64;

    let mut recs: Vec<UdfFileRec> = Vec::new();
    udf_walk_dir(f, &vol, root_lba, "", 0, &mut recs);

    let mut vfs = IsoVFS {
        path: path.to_path_buf(),
        entries: Vec::new(),
        files: HashMap::new(),
    };
    for r in recs {
        vfs.files.insert(r.path.clone(), Arc::new(r.extents));
        vfs.entries.push(VirtualEntry {
            name: r.path,
            size: r.size,
            is_dir: false,
            encrypted: false,
            crc: None,
        });
    }
    vfs.entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(vfs)
}

/// FE(261)/EFE(266) 的 AD 区定位：(ad_offset, l_ad)。
/// ICB flags（FE 偏移 34..36 的 u16）bit3-5 = 分配描述符类型：3 = 数据内嵌
/// ICB（无外部 extent，返回 None）；BD-ROM 恒用 short_ad（类型 0），
/// long_ad/ext_ad 形态按 short_ad 布局处理（字段前 8 字节兼容）
fn udf_ad_area(sector: &[u8], tag: u16) -> Option<(usize, usize)> {
    let (l_ea_off, ad_base) = match tag {
        266 => (208usize, 216usize),
        261 => (168, 176),
        _ => return None,
    };
    if l_ea_off + 4 > sector.len() {
        return None;
    }
    let l_ea = u32::from_le_bytes([
        sector[l_ea_off],
        sector[l_ea_off + 1],
        sector[l_ea_off + 2],
        sector[l_ea_off + 3],
    ]) as usize;
    let l_ad_off = l_ea_off + 4;
    let l_ad = u32::from_le_bytes([
        sector[l_ad_off],
        sector[l_ad_off + 1],
        sector[l_ad_off + 2],
        sector[l_ad_off + 3],
    ]) as usize;
    let ad_off = ad_base + l_ea;
    if ad_off + l_ad > sector.len() {
        return None;
    }
    let flags = u16::from_le_bytes([sector[34], sector[35]]);
    if (flags >> 3) & 7 == 3 {
        return None; // 内嵌数据形态：无外部 extent
    }
    Some((ad_off, l_ad))
}

/// FE/EFE → 全部数据 extent（镜像绝对字节偏移, 长度）。
/// short_ad 落在物理分区；AD 连续链（extent type 3）读指向区域续解
/// （超大 m2ts 的保险，BD 实测单 ICB 可容 ~228 条 AD ≈ 900GB，罕见）
fn udf_file_extents(f: &mut std::fs::File, vol: &UdfVol, meta_lba: u64) -> Vec<(u64, u64)> {
    let Some(first) = read_sector(f, vol.metadata_start + meta_lba) else {
        return Vec::new();
    };
    let tag = u16::from_le_bytes([first[0], first[1]]);
    let Some((ad_off, l_ad)) = udf_ad_area(&first, tag) else {
        return Vec::new();
    };
    let mut buf: Vec<u8> = first.into();
    let mut ad_off = ad_off;
    let mut l_ad = l_ad;
    let mut extents = Vec::new();
    let mut chains = 0usize;
    loop {
        let mut i = 0usize;
        let mut restart: Option<(u64, u32)> = None; // (物理分区相对 lba, 字节数)
        while i + 8 <= l_ad {
            let raw = u32::from_le_bytes([buf[ad_off + i], buf[ad_off + i + 1], buf[ad_off + i + 2], buf[ad_off + i + 3]]);
            let len = (raw & 0x3FFF_FFFF) as u64;
            let lba = u32::from_le_bytes([
                buf[ad_off + i + 4],
                buf[ad_off + i + 5],
                buf[ad_off + i + 6],
                buf[ad_off + i + 7],
            ]) as u64;
            match raw >> 30 {
                0 => {
                    if len > 0 {
                        extents.push(((vol.partition_start + lba) * SECTOR, len));
                    }
                }
                3 => {
                    // AD 连续链：本条 extent 指向下一段 AD 区域
                    if len > 0 && len <= MAX_DIR_BYTES {
                        restart = Some((lba, len as u32));
                    }
                    break;
                }
                _ => {} // 1=未记录已分配（稀疏）、2=空洞：跳过（读时按 gap 兜底）
            }
            i += 8;
        }
        match restart {
            Some((lba, len)) if chains < UDF_AD_CHAIN_MAX => {
                chains += 1;
                let abs = (vol.partition_start + lba) * SECTOR;
                let mut next = vec![0u8; len as usize];
                if f.seek(SeekFrom::Start(abs)).is_err() {
                    break;
                }
                if f.read_exact(&mut next).is_err() {
                    break;
                }
                buf = next;
                ad_off = 0;
                l_ad = buf.len();
            }
            _ => break,
        }
    }
    extents
}

/// 递归 walk 目录树：目录 ICB → 目录数据（元数据分区）→ FID 链 → 子项。
/// 解析异常（记录越界/截断）静默放弃该目录剩余部分——部分可读照常收集
fn udf_walk_dir(
    f: &mut std::fs::File,
    vol: &UdfVol,
    dir_meta_lba: u64,
    prefix: &str,
    depth: usize,
    out: &mut Vec<UdfFileRec>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let Some(icb) = read_sector(f, vol.metadata_start + dir_meta_lba) else {
        return;
    };
    let tag = u16::from_le_bytes([icb[0], icb[1]]);
    let Some((ad_off, l_ad)) = udf_ad_area(&icb, tag) else {
        return;
    };
    if l_ad < 8 {
        return;
    }
    let raw = u32::from_le_bytes([icb[ad_off], icb[ad_off + 1], icb[ad_off + 2], icb[ad_off + 3]]);
    let dir_len = (raw & 0x3FFF_FFFF) as u64;
    let pos = u32::from_le_bytes([icb[ad_off + 4], icb[ad_off + 5], icb[ad_off + 6], icb[ad_off + 7]]);
    if raw >> 30 != 0 || dir_len == 0 || dir_len > MAX_DIR_BYTES {
        return; // 目录数据多段/稀疏/超大：不支持（BD 目录恒单段）
    }
    // 目录数据在元数据分区
    let mut data = vec![0u8; dir_len as usize];
    if f.seek(SeekFrom::Start((vol.metadata_start + pos as u64) * SECTOR)).is_err() {
        return;
    }
    if f.read_exact(&mut data).is_err() {
        return;
    }
    let end = data.len();
    let mut p = 0usize;
    while p + 38 <= end {
        if u16::from_le_bytes([data[p], data[p + 1]]) != 257 {
            break; // FID 链断裂：停止解析
        }
        let chars = data[p + 18];
        let l_fi = data[p + 19] as usize;
        let icb_lba = u32::from_le_bytes([data[p + 24], data[p + 25], data[p + 26], data[p + 27]]) as u64;
        let l_iu = u16::from_le_bytes([data[p + 36], data[p + 37]]) as usize;
        let fid_len = (38 + l_iu + l_fi + 3) & !3;
        if fid_len == 0 {
            break;
        }
        let is_dir = chars & 0x02 != 0;
        let is_parent = chars & 0x08 != 0;
        let name_end = p + 38 + l_iu + l_fi;
        if !is_parent && l_fi > 0 && name_end <= end {
            let name = udf_name(&data[p + 38 + l_iu..name_end]);
            if !name.is_empty() {
                let child_path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if is_dir {
                    udf_walk_dir(f, vol, icb_lba, &child_path, depth + 1, out);
                } else {
                    // 文件：ICB 读 info_length + 全部数据 extent（物理分区）
                    let size = read_sector(f, vol.metadata_start + icb_lba)
                        .map(|s| {
                            let t = u16::from_le_bytes([s[0], s[1]]);
                            if t == 261 || t == 266 {
                                u64::from_le_bytes([
                                    s[56], s[57], s[58], s[59], s[60], s[61], s[62], s[63],
                                ])
                            } else {
                                0
                            }
                        })
                        .unwrap_or(0);
                    let extents = udf_file_extents(f, vol, icb_lba);
                    out.push(UdfFileRec { path: child_path, size, extents });
                }
            }
        }
        p += fid_len;
    }
}

/// UDF 文件名：首字节 = 压缩 ID（8 = ASCII，16 = UTF-16BE），CS0 以 0/空格填充
fn udf_name(raw: &[u8]) -> String {
    match raw.first() {
        Some(&8) => String::from_utf8_lossy(&raw[1..])
            .trim_end_matches(['\0', ' '])
            .to_string(),
        Some(&16) => raw[1..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .filter(|&u| u != 0)
            .map(|u| char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
            .collect(),
        _ => String::from_utf8_lossy(raw).trim_end_matches('\0').to_string(),
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
    use std::collections::BTreeMap;

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

    // ---- UDF 2.50 fixture：程序化构造纯 UDF 蓝光形态镜像（257 扇区） ----
    // 布局：32=PVD(1) 33=PD(5,物理分区起点 40) 34=LVD(6,type2 元数据映射→
    // meta_file_lbn) 35=TD(8)；40+meta_file_lbn=元数据文件 EFE（AD→内容 41 起
    // 14 扇区）；41=FSD(根 ICB meta1)；42=根目录 EFE→数据 meta3；44=根 FID 链
    // （BDMV icb meta4 / README.TXT meta12 / 自述.txt meta13）；45=BDMV EFE→
    // 数据 meta6；47=BDMV FID 链（STREAM meta7）；48=STREAM EFE→数据 meta9；
    // 50=STREAM FID 链（00000.M2TS icb meta11）；52=M2TS EFE 双 extent
    // （物理分区相对 20、22→绝对 60、62 跨扇区）；53=README FE(261, extent@30)；
    // 54=自述 EFE（extent@31）；256=AVDP(2)。
    // meta_file_lbn=0 → 退化路径（map 位置 0 试 lba 0）；15 → 主路径（map 位置字段生效）
    fn udf_tag(s: &mut [u8], tag: u16) {
        s[0..2].copy_from_slice(&tag.to_le_bytes());
        s[2..4].copy_from_slice(&3u16.to_le_bytes());
    }

    /// FE(261)/EFE(266)：info_length 在 56，AD 区基座 261=176 / 266=216
    fn udf_icb(tag: u16, info_len: u64, ads: &[(u32, u32)], is_dir: bool) -> [u8; 2048] {
        let mut s = [0u8; 2048];
        udf_tag(&mut s, tag);
        // ICB tag（16..36）：file_type（16+11=27），flags（16+18=34）= 0（short_ad）
        s[27] = if is_dir { 5 } else { 4 };
        s[56..64].copy_from_slice(&info_len.to_le_bytes());
        let (l_ea_off, ad_base) = if tag == 266 { (208, 216) } else { (168, 176) };
        s[l_ea_off..l_ea_off + 4].copy_from_slice(&0u32.to_le_bytes());
        s[l_ea_off + 4..l_ea_off + 8].copy_from_slice(&((ads.len() * 8) as u32).to_le_bytes());
        for (i, (raw, lba)) in ads.iter().enumerate() {
            let off = ad_base + i * 8;
            s[off..off + 4].copy_from_slice(&raw.to_le_bytes());
            s[off + 4..off + 8].copy_from_slice(&lba.to_le_bytes());
        }
        s
    }

    /// FID(257)：chars bit1=目录 bit3=父项；ICB long_ad 在 20..36（lba 在 24..28）
    fn udf_fid(name: &[u8], is_dir: bool, is_parent: bool, icb_meta_lba: u32) -> Vec<u8> {
        let len = (38 + name.len() + 3) & !3;
        let mut r = vec![0u8; len];
        r[0..2].copy_from_slice(&257u16.to_le_bytes());
        r[2..4].copy_from_slice(&3u16.to_le_bytes());
        r[18] = (if is_parent { 0x08 } else { 0 }) | (if is_dir { 0x02 } else { 0 });
        r[19] = name.len() as u8;
        r[20..24].copy_from_slice(&2048u32.to_le_bytes());
        r[24..28].copy_from_slice(&icb_meta_lba.to_le_bytes());
        r[28..30].copy_from_slice(&1u16.to_le_bytes()); // partref = 元数据分区
        r[38..38 + name.len()].copy_from_slice(name);
        r
    }

    fn ascii_name(s: &str) -> Vec<u8> {
        let mut v = vec![8u8];
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn utf16_name(s: &str) -> Vec<u8> {
        let mut v = vec![16u8];
        v.extend(s.encode_utf16().flat_map(|u| u.to_be_bytes()));
        v
    }

    fn build_udf_iso(meta_file_lbn: u32) -> Vec<u8> {
        const B: usize = 2048;
        let mut img = vec![vec![0u8; B]; 257];

        let m2ts_a: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let m2ts_b: Vec<u8> = (0..500u32).map(|i| (i % 253) as u8).collect();
        let readme = b"hello udf\n".to_vec();
        let notes: Vec<u8> = (0..200u32).map(|i| (i * 7 % 256) as u8).collect();
        // extent1（3000 字节）跨扇区 60、61；extent2 独立占扇区 62
        img[60].copy_from_slice(&m2ts_a[..2048]);
        img[61][..952].copy_from_slice(&m2ts_a[2048..]);
        img[62][..500].copy_from_slice(&m2ts_b);
        img[70][..readme.len()].copy_from_slice(&readme);
        img[71][..notes.len()].copy_from_slice(&notes);

        // VDS（32..36）
        let mut pvd = [0u8; B];
        udf_tag(&mut pvd, 1);
        pvd[24] = 8;
        pvd[25..29].copy_from_slice(b"TEST");
        pvd[55] = 5; // dstring 尾部长度字节
        img[32].copy_from_slice(&pvd);
        let mut pd = [0u8; B];
        udf_tag(&mut pd, 5);
        pd[188..192].copy_from_slice(&40u32.to_le_bytes());
        img[33].copy_from_slice(&pd);
        let mut lvd = [0u8; B];
        udf_tag(&mut lvd, 6);
        lvd[268..272].copy_from_slice(&2u32.to_le_bytes());
        // map0 = type1（物理分区 pn 0）
        lvd[440] = 1;
        lvd[441] = 6;
        lvd[442..444].copy_from_slice(&1u16.to_le_bytes());
        lvd[444..446].copy_from_slice(&0u16.to_le_bytes());
        // map1 = type2 元数据分区映射（446 起，长 64，真盘布局）：4..36 =
        // EntityID（flags + "*UDF Metadata Partition" + 0x0250 后缀），
        // 40 = Metadata File Location（物理分区相对 u32）
        lvd[446] = 2;
        lvd[447] = 64;
        lvd[448..450].copy_from_slice(&1u16.to_le_bytes());
        lvd[450] = 0; // entity flags
        lvd[451..474].copy_from_slice(b"*UDF Metadata Partition");
        lvd[474..476].copy_from_slice(&0x0250u16.to_le_bytes());
        lvd[478..480].copy_from_slice(&1u16.to_le_bytes());
        lvd[480..482].copy_from_slice(&0u16.to_le_bytes());
        lvd[486..490].copy_from_slice(&meta_file_lbn.to_le_bytes());
        img[34].copy_from_slice(&lvd);
        let mut td = [0u8; B];
        udf_tag(&mut td, 8);
        img[35].copy_from_slice(&td);

        // 元数据文件 EFE @物理分区相对 meta_file_lbn：内容 = 元数据分区（41 起 14 扇区）
        img[40 + meta_file_lbn as usize]
            .copy_from_slice(&udf_icb(266, (14 * B) as u64, &[(14 * B as u32, 1)], false));

        // FSD @41（meta 0）→ 根 ICB meta 1
        let mut fsd = [0u8; B];
        udf_tag(&mut fsd, 256);
        fsd[400..404].copy_from_slice(&2048u32.to_le_bytes());
        fsd[404..408].copy_from_slice(&1u32.to_le_bytes());
        img[41].copy_from_slice(&fsd);

        // 目录数据
        let mut root_data = Vec::new();
        root_data.extend(udf_fid(&[], false, true, 0));
        root_data.extend(udf_fid(&ascii_name("BDMV"), true, false, 4));
        root_data.extend(udf_fid(&ascii_name("README.TXT"), false, false, 12));
        root_data.extend(udf_fid(&utf16_name("自述.txt"), false, false, 13));
        let mut bdmv_data = Vec::new();
        bdmv_data.extend(udf_fid(&[], false, true, 0));
        bdmv_data.extend(udf_fid(&ascii_name("STREAM"), true, false, 7));
        let mut stream_data = Vec::new();
        stream_data.extend(udf_fid(&[], false, true, 0));
        stream_data.extend(udf_fid(&ascii_name("00000.M2TS"), false, false, 11));

        let mut sec = [0u8; B];
        sec[..root_data.len()].copy_from_slice(&root_data);
        img[44].copy_from_slice(&sec);
        let mut sec = [0u8; B];
        sec[..bdmv_data.len()].copy_from_slice(&bdmv_data);
        img[47].copy_from_slice(&sec);
        let mut sec = [0u8; B];
        sec[..stream_data.len()].copy_from_slice(&stream_data);
        img[50].copy_from_slice(&sec);

        // 目录 ICB：AD 单段指向元数据相对 lba
        img[42].copy_from_slice(&udf_icb(266, root_data.len() as u64, &[(root_data.len() as u32, 3)], true));
        img[45].copy_from_slice(&udf_icb(266, bdmv_data.len() as u64, &[(bdmv_data.len() as u32, 6)], true));
        img[48].copy_from_slice(&udf_icb(266, stream_data.len() as u64, &[(stream_data.len() as u32, 9)], true));
        // M2TS：双 extent 测多 extent + 跨 extent seek
        img[52].copy_from_slice(&udf_icb(266, 3500, &[(3000, 20), (500, 22)], false));
        // README 用 FE(tag 261) 覆盖 FE 分支；自述 EFE 单 extent
        img[53].copy_from_slice(&udf_icb(261, readme.len() as u64, &[(readme.len() as u32, 30)], false));
        img[54].copy_from_slice(&udf_icb(266, notes.len() as u64, &[(notes.len() as u32, 31)], false));

        // AVDP @256
        let mut avdp = [0u8; B];
        udf_tag(&mut avdp, 2);
        avdp[16..20].copy_from_slice(&32u32.to_le_bytes());
        avdp[20..24].copy_from_slice(&(4 * B as u32).to_le_bytes());
        img[256].copy_from_slice(&avdp);

        img.concat()
    }

    #[test]
    fn udf蓝光镜像_列表_多extent读取_跨extentseek() {
        // 两种元数据文件定位路径都要过：map 位置字段生效（15）/ 退化试 lba 0（0）
        for meta_file_lbn in [15u32, 0] {
            run_udf_suite(meta_file_lbn);
        }
    }

    fn run_udf_suite(meta_file_lbn: u32) {
        let img = build_udf_iso(meta_file_lbn);
        let path = tmp_iso("udf");
        std::fs::write(&path, &img).unwrap();
        let vfs = IsoVFS::open(&path, None).unwrap();
        let list = vfs.list();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["BDMV/STREAM/00000.M2TS", "README.TXT", "自述.txt"]);
        assert_eq!(list[0].size, 3500);
        assert_eq!(list[1].size, 10);
        assert_eq!(list[2].size, 200);
        for e in &list {
            assert!(!e.is_dir && !e.encrypted && e.crc.is_none());
        }
        assert_eq!(vfs.access_mode(), AccessMode::FastSeek);

        // M2TS 双 extent 顺序读
        let mut vf = vfs.open_entry(&list[0]).unwrap();
        let mut buf = Vec::new();
        vf.read_to_end(&mut buf).unwrap();
        let expect_m2ts: Vec<u8> = (0..3000u32)
            .map(|i| (i % 251) as u8)
            .chain((0..500u32).map(|i| (i % 253) as u8))
            .collect();
        assert_eq!(buf, expect_m2ts);

        // 跨 extent seek：extent1 = 3000 字节，读 2990..3010 跨界
        vf.seek(SeekFrom::Start(2990)).unwrap();
        let mut mid = [0u8; 20];
        vf.read_exact(&mut mid).unwrap();
        assert_eq!(&mid, &expect_m2ts[2990..3010]);

        // FE(261) 分支 + UTF-16BE 名字条目
        let mut vf = vfs.open_entry(&list[1]).unwrap();
        let mut buf = Vec::new();
        vf.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello udf\n");
        let mut vf = vfs.open_entry(&list[2]).unwrap();
        let mut buf = Vec::new();
        vf.read_to_end(&mut buf).unwrap();
        let expect_notes: Vec<u8> = (0..200u32).map(|i| (i * 7 % 256) as u8).collect();
        assert_eq!(buf, expect_notes);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    #[ignore] // 手动真盘诊断：$env:ARCHVIEW_ISO_TEST="<iso路径>"; cargo test 真盘诊断 -- --ignored --nocapture
    fn iso_vfs真盘诊断() {
        let Some(path) = std::env::var_os("ARCHVIEW_ISO_TEST") else {
            return;
        };
        let vfs = IsoVFS::open(Path::new(&path), None).unwrap();
        let list = vfs.list();
        println!("条目 {} 个", list.len());
        for e in list.iter().take(20) {
            println!("  {} ({} B)", e.name, e.size);
        }
        // 数据平面验证：全部 m2ts 读段。BDAV MPEG-TS = 192B 节奏（4B
        // TP_extra_header + 188B TS 包），0x47 同步字节在每 192B 单元偏移 4
        for v in list.iter().filter(|e| {
            e.name.to_ascii_lowercase().ends_with(".m2ts")
        }) {
            let mut vf = match vfs.open_entry(v) {
                Ok(vf) => vf,
                Err(e) => {
                    println!("  {} ({} B) 打开失败: {e}", v.name, v.size);
                    continue;
                }
            };
            let mut head = [0u8; 192 * 2];
            match vf.read_exact(&mut head) {
                Ok(_) => {
                    let head_ok = head.chunks_exact(192).all(|p| p[4] == 0x47);
                    let mid = (v.size / 2 / 192) * 192;
                    vf.seek(SeekFrom::Start(mid)).unwrap();
                    let mut midbuf = [0u8; 192 * 2];
                    vf.read_exact(&mut midbuf).unwrap();
                    let mid_ok = midbuf.chunks_exact(192).all(|p| p[4] == 0x47);
                    println!("  {head_ok} {mid_ok} {} ({} B)", v.name, v.size);
                }
                Err(e) => println!("  {} ({} B) 读失败: {e}", v.name, v.size),
            }
        }
    }
}
