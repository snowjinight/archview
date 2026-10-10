//! zvf:// 协议桥：mpv stream_cb 回调 ↔ VFS 层。
//! URI 格式：zvf://<archive_id>/<entry_idx>
//! 回调在 mpv 内部线程执行 —— 回调内禁止调用任何 mpv API（死锁）。

use crate::player::mpv_ffi as ffi;
use crate::vfs::{ArchiveVFS, VirtualEntry};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

struct ArchiveHandle {
    /// 压缩包路径：register 冲突探测时区分"同包重开"（复用 id）与"哈希碰撞"（换 id）
    path: std::path::PathBuf,
    vfs: Arc<dyn ArchiveVFS>,
    entries: Vec<VirtualEntry>,
}

/// 全局流注册表：UI 线程注册，mpv 回调线程查找。
static REGISTRY: Mutex<Option<HashMap<u64, ArchiveHandle>>> = Mutex::new(None);

/// 压缩包稳定 ID：路径哈希（自增 ID 会导致 A→B→A 重开/程序重启时缓存路径变化、无法命中）
fn archive_stable_id(path: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

/// 注册压缩包，返回 (archive_id, entries)。entries 由 VFS 列出并缓存于注册表。
/// id = 路径哈希（稳定，缓存按路径命中）；同路径重开复用原 id 覆盖旧注册项；
/// 极小概率哈希碰撞（不同路径同 id）时线性探测换 id，避免两包共抢注册项/缓存前缀。
pub fn register(vfs: Arc<dyn ArchiveVFS>, path: &std::path::Path) -> (u64, Vec<VirtualEntry>) {
    let base = archive_stable_id(path);
    let entries = vfs.list();
    let mut g = REGISTRY.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    let mut id = base;
    loop {
        match m.get(&id) {
            // 同包重开（含带密码重开）：复用 id，覆盖注册项
            Some(h) if h.path == path => break,
            // 哈希碰撞：不同路径撞出同 id，探测下一个
            Some(_) => id = id.wrapping_add(1),
            None => break,
        }
    }
    m.insert(
        id,
        ArchiveHandle {
            path: path.to_path_buf(),
            vfs,
            entries: entries.clone(),
        },
    );
    (id, entries)
}

pub fn entry_uri(archive_id: u64, entry_idx: usize) -> String {
    format!("zvf://{archive_id}/{entry_idx}")
}

// ---- mpv stream_cb 四个回调 ----
// cookie 约定：Box<dyn VirtualFile> 的裸指针，close 时 Box::from_raw 回收。

pub(crate) unsafe extern "C" fn open_cb(
    _user_data: *mut c_void,
    uri: *mut c_char,
    info: *mut ffi::StreamCbInfo,
) -> i32 {
    let uri = CStr::from_ptr(uri).to_string_lossy();
    let Some(rest) = uri.strip_prefix("zvf://") else {
        return ffi::MPV_ERROR_LOADING_FAILED;
    };
    let Some((a, e)) = rest.split_once('/') else {
        return ffi::MPV_ERROR_LOADING_FAILED;
    };
    let (Ok(archive_id), Ok(entry_idx)) = (a.parse::<u64>(), e.parse::<usize>()) else {
        return ffi::MPV_ERROR_LOADING_FAILED;
    };

    let (vfs, entry) = {
        let g = REGISTRY.lock().unwrap(); // 与 UI 线程短暂争锁，安全
        match g.as_ref().and_then(|m| m.get(&archive_id)) {
            Some(h) => (h.vfs.clone(), h.entries.get(entry_idx).cloned()),
            None => return ffi::MPV_ERROR_LOADING_FAILED,
        }
    };
    let Some(entry) = entry else {
        return ffi::MPV_ERROR_LOADING_FAILED;
    };
    let vfile: Box<dyn crate::vfs::VirtualFile> = match vfs.open_entry(&entry) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("open_entry 失败 {}: {e}", entry.name);
            return ffi::MPV_ERROR_LOADING_FAILED;
        }
    };

    // 双重装箱：外层 Box 是瘦指针，cookie 指向它；内层 Box<dyn> 是胖指针。
    // 若直接 Box::into_raw(Box<dyn>) 会得到胖指针，as *mut c_void 丢掉 vtable，回调无法重建。
    (*info).cookie = Box::into_raw(Box::new(vfile)) as *mut c_void;
    (*info).read_fn = Some(read_cb);
    (*info).seek_fn = Some(seek_cb); // mpv 打开后会立刻 seek(0) 探测可 seek 性
    (*info).size_fn = Some(size_cb);
    (*info).close_fn = Some(close_cb);
    (*info).cancel_fn = None;
    ffi::MPV_ERROR_SUCCESS
}

unsafe extern "C" fn read_cb(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64 {
    if cookie.is_null() {
        return -1;
    }
    let f = &mut *(cookie as *mut Box<dyn crate::vfs::VirtualFile>);
    let slice = std::slice::from_raw_parts_mut(buf as *mut u8, nbytes as usize);
    match f.read(slice) {
        Ok(n) => n as i64,
        Err(e) => {
            tracing::error!("stream_cb read 失败: {e}");
            -1
        }
    }
}

unsafe extern "C" fn seek_cb(cookie: *mut c_void, offset: i64) -> i64 {
    if cookie.is_null() {
        return ffi::MPV_ERROR_UNSUPPORTED as i64;
    }
    let f = &mut *(cookie as *mut Box<dyn crate::vfs::VirtualFile>);
    match f.seek(SeekFrom::Start(offset.max(0) as u64)) {
        Ok(pos) => pos as i64,
        Err(_) => ffi::MPV_ERROR_UNSUPPORTED as i64,
    }
}

unsafe extern "C" fn size_cb(cookie: *mut c_void) -> i64 {
    if cookie.is_null() {
        return -1;
    }
    (*(cookie as *mut Box<dyn crate::vfs::VirtualFile>)).size() as i64
}

unsafe extern "C" fn close_cb(cookie: *mut c_void) {
    if !cookie.is_null() {
        drop(Box::from_raw(cookie as *mut Box<dyn crate::vfs::VirtualFile>));
    }
}
