//! 缓存管理：7z/RAR 后台解压的目标目录 + 总量上限（LRU 按 mtime 驱逐）。
//! 目录来自 config.toml（"auto" = 系统临时目录），代码零硬编码路径。

use crate::config::Config;
use crate::vfs::VirtualEntry;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 解压缓存目录（不存在则创建）
pub fn cache_dir(cfg: &Config) -> PathBuf {
    let dir = if cfg.cache.dir == "auto" {
        std::env::temp_dir().join("archview_cache")
    } else {
        PathBuf::from(&cfg.cache.dir)
    };
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// 目录所在磁盘的剩余字节数（Windows GetDiskFreeSpaceExW，微秒级；
/// 目录不可达/查询失败返回 None，调用方按"无法判断"处理）
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut avail: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

/// 缓存目录当前占用字节数（只统计普通文件；目录不可达按 0）
pub fn dir_usage(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// 计划写入 incoming_bytes 前，确保总量在上限内：超出则按 mtime 从旧到新删除。
/// 返回被删除文件的 (archive_id, entry_idx)（文件名解析不出 idx 的跳过上报），
/// 调用方据此清缓存命中位图（见 scan_cached）
pub fn evict_if_needed(dir: &Path, max_bytes: u64, incoming_bytes: u64) -> Vec<(u64, usize)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, u64, SystemTime)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((e.path(), meta.len(), mtime))
        })
        .collect();
    files.sort_by_key(|(_, _, t)| *t); // 旧在前

    let total: u64 = files.iter().map(|(_, s, _)| s).sum();
    let mut over = total
        .saturating_add(incoming_bytes)
        .saturating_sub(max_bytes);
    let mut evicted = Vec::new();
    if over == 0 {
        return evicted;
    }
    for (path, size, _) in files {
        if over == 0 {
            break;
        }
        tracing::debug!("驱逐缓存: {}", path.display());
        if std::fs::remove_file(&path).is_ok() {
            over = over.saturating_sub(size);
            if let Some(k) = parse_cache_name(&path) {
                evicted.push(k);
            }
        }
    }
    evicted
}

/// 从缓存文件名解析 (archive_id, entry_idx)：{aid}_{idx}_{basename}。
/// 解析失败（非缓存命名/手放文件）返回 None
fn parse_cache_name(path: &Path) -> Option<(u64, usize)> {
    let name = path.file_name()?.to_str()?;
    let (aid, rest) = name.split_once('_')?;
    let (idx, _) = rest.split_once('_')?;
    Some((aid.parse().ok()?, idx.parse().ok()?))
}

/// 扫描缓存目录，构建指定包的"条目已落地"位图（下标 = entry idx）。
/// 打开包时调用一次，此后批扫描/命中判定查位图——万级条目批扫描从
/// N 次 per-file metadata 系统调用降到 0 次；read_dir 附带的
/// DirEntry::metadata 零额外打开（NTFS/网络盘都远快于逐文件 metadata）。
/// 尺寸与条目一致才置位（防半截/残留错位文件）。不按扩展名排除临时文件：
/// tmp_path 的 ~tmp_ 前缀与其 {aid}_ 解析天然不冲突；条目本名叫 xxx.part1
/// 的落地缓存扩展名就是 part1，是合法缓存必须收下（启动时残留已由
/// cleanup_partial 清掉，扫描时不会遇到临时文件）
pub fn scan_cached(dir: &Path, archive_id: u64, entries: &[VirtualEntry]) -> Vec<bool> {
    let prefix = format!("{archive_id}_");
    let mut out = vec![false; entries.len()];
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let fname = e.file_name();
        let Some(name) = fname.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some((idx_str, _base)) = rest.split_once('_') else {
            continue;
        };
        let Ok(idx) = idx_str.parse::<usize>() else {
            continue;
        };
        let Some(entry) = entries.get(idx) else {
            continue;
        };
        if e.metadata().map(|m| m.len() == entry.size).unwrap_or(false) {
            out[idx] = true;
        }
    }
    out
}

/// 目标缓存文件名：{archive_id}_{entry_idx}_{basename}（防不同目录同名冲突）
pub fn dest_path(dir: &Path, archive_id: u64, entry_idx: usize, entry_name: &str) -> PathBuf {
    let base = entry_name.rsplit('/').next().unwrap_or(entry_name);
    let safe: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{archive_id}_{entry_idx}_{safe}"))
}

/// 启动时清理解压半截临时文件（上次解压中断/并发失败的残留）：
/// ~tmp_ 前缀形态。不能按扩展名一刀切——条目本名可能就叫 xxx.part1
/// （分卷文件），其落地缓存扩展名就是 part1，是合法缓存不是残留
pub fn cleanup_partial(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let is_tmp = e
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("~tmp_"));
        if is_tmp {
            std::fs::remove_file(&p).ok();
        }
    }
}

/// 清空整个缓存目录（保留目录本身），返回释放的字节数（尽力而为，占用中的文件跳过）
pub fn clean_all(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut freed = 0u64;
    for e in rd.flatten() {
        let p = e.path();
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        let ok = if p.is_dir() {
            std::fs::remove_dir_all(&p).is_ok()
        } else {
            std::fs::remove_file(&p).is_ok()
        };
        if ok {
            freed += size;
        }
    }
    freed
}

/// 删除指定压缩包解出的缓存文件（文件名 {archive_id}_ 前缀），返回释放的字节数。
/// 打开新压缩包时调用，避免旧包缓存无限累积。
pub fn clean_archive(dir: &Path, archive_id: u64) -> u64 {
    let prefix = format!("{archive_id}_");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut freed = 0u64;
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
        if !name.as_deref().is_some_and(|n| n.starts_with(&prefix)) {
            continue;
        }
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        if std::fs::remove_file(&p).is_ok() {
            freed += size;
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, size: u64) -> VirtualEntry {
        VirtualEntry {
            name: name.into(),
            size,
            is_dir: false,
            encrypted: false,
            crc: None,
        }
    }

    /// 条目本名叫 xxx.part1（分卷文件）的落地缓存不得被当临时文件：
    /// cleanup_partial 启动清理必须保留它、scan_cached 必须收下；
    /// ~tmp_ 前缀临时文件则应清掉
    #[test]
    fn part_named_entry_cache_survives_cleanup_and_scans() {
        let dir = std::env::temp_dir().join(format!("archview_cache_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let entries = vec![entry("movie.part1", 10)];
        let dest = dest_path(&dir, 7, 0, "movie.part1");
        std::fs::write(&dest, vec![9u8; 10]).unwrap();

        let tmp = crate::vfs::tmp_path(&dest);
        assert_eq!(
            tmp.file_name().unwrap().to_str().unwrap().split_once('_').unwrap().0,
            "~tmp",
            "临时文件前缀形态"
        );
        std::fs::write(&tmp, b"half").unwrap();

        cleanup_partial(&dir);
        assert!(!tmp.exists(), "~tmp_ 前缀临时文件应被清理");
        assert!(dest.exists(), "xxx.part1 条目的落地缓存是合法缓存，不得清理");

        let bitmap = scan_cached(&dir, 7, &entries);
        assert!(bitmap[0], "part 命名条目的落地缓存应命中位图");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
