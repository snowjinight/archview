//! 缓存管理：7z/RAR 后台解压的目标目录 + 总量上限（LRU 按 mtime 驱逐）。
//! 目录来自 config.toml（"auto" = 系统临时目录），代码零硬编码路径。

use crate::config::Config;
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

/// 计划写入 incoming_bytes 前，确保总量在上限内：超出则按 mtime 从旧到新删除
pub fn evict_if_needed(dir: &Path, max_bytes: u64, incoming_bytes: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
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
    if over == 0 {
        return;
    }
    for (path, size, _) in files {
        if over == 0 {
            break;
        }
        tracing::debug!("驱逐缓存: {}", path.display());
        if std::fs::remove_file(&path).is_ok() {
            over = over.saturating_sub(size);
        }
    }
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

/// 启动时清理 .part / .partN 半截临时文件（上次解压中断/并发失败的残留）
pub fn cleanup_partial(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let is_tmp = p
            .extension()
            .map(|x| x.to_string_lossy().starts_with("part"))
            .unwrap_or(false);
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
