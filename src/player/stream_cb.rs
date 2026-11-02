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
/// list 期间的进度经 progress 回调上报（后台打开线程转发给 UI；不关心进度传空回调）
pub fn register_with_progress(
    vfs: Arc<dyn ArchiveVFS>,
    path: &std::path::Path,
    progress: &dyn Fn(Option<f32>, &str),
) -> (u64, Vec<VirtualEntry>) {
    let base = archive_stable_id(path);
    let entries = vfs.list_with_progress(progress);
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

pub fn entry_uri(archive_id: u64, entry_idx: usize, bit_shift: u32) -> String {
    // shift>0 时以查询串携带（zvf:// 是自有协议，mpv 将完整 URI 原样
    // 交给 open_cb，不解析查询串；不同 shift → 不同 URI → 缓存天然隔离）
    if bit_shift > 0 {
        format!("zvf://{archive_id}/{entry_idx}?s={bit_shift}")
    } else {
        format!("zvf://{archive_id}/{entry_idx}")
    }
}

/// 解析 zvf:// URI → (archive_id, entry_idx, bit_shift)。查询串仅接受
/// 本协议定义的 `s=<0..8>`，出现其它键/坏值直接判非法（URI 只由本文件
/// 生成，宽松解析反而掩盖拼装 bug）
fn parse_uri(uri: &str) -> Option<(u64, usize, u32)> {
    let rest = uri.strip_prefix("zvf://")?;
    let (main, query) = match rest.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (rest, None),
    };
    let shift = match query {
        None => 0,
        Some(q) => {
            let v = q.strip_prefix("s=")?;
            v.parse::<u32>().ok().filter(|s| (1..=8).contains(s))?
        }
    };
    let (a, e) = main.split_once('/')?;
    Some((a.parse::<u64>().ok()?, e.parse::<usize>().ok()?, shift))
}

// ---- mpv stream_cb 四个回调 ----
// cookie 约定：Box<dyn VirtualFile> 的裸指针，close 时 Box::from_raw 回收。

pub(crate) unsafe extern "C" fn open_cb(
    _user_data: *mut c_void,
    uri: *mut c_char,
    info: *mut ffi::StreamCbInfo,
) -> i32 {
    let uri = CStr::from_ptr(uri).to_string_lossy();
    let Some((archive_id, entry_idx, bit_shift)) = parse_uri(&uri) else {
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
    // bit_shift>0：包一层流层采样修正（12bit 低对齐数据发黑修复）
    let vfile = if bit_shift > 0 {
        Box::new(ShiftReader::new(vfile, bit_shift)) as Box<dyn crate::vfs::VirtualFile>
    } else {
        vfile
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
        drop(Box::from_raw(
            cookie as *mut Box<dyn crate::vfs::VirtualFile>,
        ));
    }
}

// ---- ShiftReader：流层 per-u16 采样左移 ----

/// 流层 bit 修正：把每个 **LE u16 采样** 左移 N 位（高位溢出丢弃，即
/// `wrapping_shl` 语义）。适用：12bit 等有效位深数据 LSB 对齐（低对齐
/// 补零）存 16bit 容器——FFmpeg bayer16* 家族转 RGB 写死取高 8 位
/// （swscale bayer_template.c `>>BAYER_SHIFT(8)`），低对齐数据只剩 16
/// 级灰阶（播放发黑根因）；左移 4 位后高 8 位恢复 12bit 有效数据。
///
/// 对齐关键：mpv read 回调的 buf 起点可为任意文件偏移，必须按**绝对
/// 偶地址**配对采样，否则同一像素会随读取切分方式漂移。自跟踪逻辑
/// 偏移 `pos`（read 累加 / seek 对齐），每次 read 从 `pos & !1` 起读，
/// 首尾各带最多 1 字节伙伴字节（回看/前看），变换后裁出交付段。
/// raw 帧大小恒为偶数，实际读取天然 2 字节对齐，此处按任意偏移通用
/// 实现以求稳健。
struct ShiftReader {
    inner: Box<dyn crate::vfs::VirtualFile>,
    shift: u32,
    /// 逻辑偏移（= 内层文件偏移，移位不改变大小）
    pos: u64,
}

impl ShiftReader {
    fn new(inner: Box<dyn crate::vfs::VirtualFile>, shift: u32) -> Self {
        debug_assert!((1..=8).contains(&shift));
        // open_entry 契约：新开句柄位于 0（DirVFS/ZipVFS 同此）
        Self { inner, shift, pos: 0 }
    }
}

impl Read for ShiftReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let pos = self.pos;
        let back = (pos & 1) as usize; // 奇地址：回看 1 字节补齐配对
        // 窗口 = [pos & !1, pos&!1 + back + buf.len() + 1)：+1 是 buf 末
        // 字节若落奇地址时的前看伙伴（文件尾读不到则少 1，尾部奇字节
        // 保持原样交付——帧数据恒偶，仅 EOF 兜底路径会走到）
        let window_len = back + buf.len() + 1;
        self.inner.seek(SeekFrom::Start(pos & !1))?;
        let mut window = vec![0u8; window_len];
        let mut got = 0;
        while got < window_len {
            match self.inner.read(&mut window[got..])? {
                0 => break, // EOF：短窗口照常变换交付
                n => got += n,
            }
        }
        if got <= back {
            // 新数据为零（EOF）：还原游标，read 契约返回 Ok(0)
            self.inner.seek(SeekFrom::Start(pos))?;
            return Ok(0);
        }
        // 变换窗口内全部完整 u16 对（高位溢出丢弃 = 期望语义）
        for i in 0..got / 2 {
            let v = u16::from_le_bytes([window[2 * i], window[2 * i + 1]]);
            let s = v.wrapping_shl(self.shift);
            window[2 * i] = s as u8;
            window[2 * i + 1] = (s >> 8) as u8;
        }
        let n = (got - back).min(buf.len());
        buf[..n].copy_from_slice(&window[back..back + n]);
        self.pos += n as u64;
        // 还原内层游标到逻辑位置（下次 read 自行重新对齐 seek）
        self.inner.seek(SeekFrom::Start(pos + n as u64))?;
        Ok(n)
    }
}

impl Seek for ShiftReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let p = self.inner.seek(pos)?;
        self.pos = p; // 大小不变，逻辑偏移 = 内层偏移
        Ok(p)
    }
}

impl crate::vfs::VirtualFile for ShiftReader {
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::VirtualFile;
    use std::io::Cursor;

    /// 测试用内存 VirtualFile（open_entry 契约同款：起始偏移 0）
    struct MemFile(Cursor<Vec<u8>>);

    impl Read for MemFile {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Seek for MemFile {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }
    impl crate::vfs::VirtualFile for MemFile {
        fn size(&self) -> u64 {
            self.0.get_ref().len() as u64
        }
    }

    fn shifted(data: &[u16], shift: u32) -> Vec<u8> {
        data.iter()
            .flat_map(|v| v.wrapping_shl(shift).to_le_bytes())
            .collect()
    }

    fn samples() -> Vec<u16> {
        // 覆盖典型值：12bit 低对齐样本 / 满量程 / 小值 / 移位后溢出项
        vec![0x0ABC, 0x0FFF, 0x0010, 0xFFFF, 0x0001, 0x0800]
    }

    #[test]
    fn uri_roundtrip_and_strictness() {
        assert_eq!(entry_uri(7, 2, 0), "zvf://7/2");
        assert_eq!(entry_uri(7, 2, 4), "zvf://7/2?s=4");
        assert_eq!(parse_uri("zvf://7/2"), Some((7, 2, 0)));
        assert_eq!(parse_uri("zvf://7/2?s=4"), Some((7, 2, 4)));
        assert_eq!(parse_uri("zvf://0/0"), Some((0, 0, 0)));
        // 非法：坏键 / 越界 shift / 坏数字 / 非本协议
        assert_eq!(parse_uri("zvf://7/2?x=4"), None);
        assert_eq!(parse_uri("zvf://7/2?s=0"), None);
        assert_eq!(parse_uri("zvf://7/2?s=9"), None);
        assert_eq!(parse_uri("zvf://a/2"), None);
        assert_eq!(parse_uri("http://7/2"), None);
        // 生成→解析全量往返
        for s in [1u32, 4, 8] {
            assert_eq!(parse_uri(&entry_uri(42, 9, s)), Some((42, 9, s)));
        }
    }

    /// 整读（偶地址起点 + 大缓冲）＝参考变换结果逐字节一致
    #[test]
    fn shift_reader_full_read() {
        let raw: Vec<u8> = samples().iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut r = ShiftReader::new(Box::new(MemFile(Cursor::new(raw.clone()))), 4);
        assert_eq!(r.size(), raw.len() as u64);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, shifted(&samples(), 4));
    }

    /// 逐 1 字节读（最恶意的切分）：与整读结果一致（跨调用配对稳定）
    #[test]
    fn shift_reader_byte_by_byte() {
        let raw: Vec<u8> = samples().iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut r = ShiftReader::new(Box::new(MemFile(Cursor::new(raw))), 4);
        let mut out = Vec::new();
        loop {
            let mut b = [0u8; 1];
            if r.read(&mut b).unwrap() == 0 {
                break;
            }
            out.push(b[0]);
        }
        assert_eq!(out, shifted(&samples(), 4));
    }

    /// 任意奇地址起点 + 任意长度读、Seek 三种基准：窗口配对不漂移
    #[test]
    fn shift_reader_odd_offset_and_seek() {
        let raw: Vec<u8> = samples().iter().flat_map(|v| v.to_le_bytes()).collect();
        let want = shifted(&samples(), 6);
        let mut r = ShiftReader::new(Box::new(MemFile(Cursor::new(raw))), 6);
        // 奇地址起点读 3 字节
        r.seek(SeekFrom::Start(1)).unwrap();
        let mut b = [0u8; 3];
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b, &want[1..4]);
        // Current/End 基准
        r.seek(SeekFrom::Current(-2)).unwrap();
        let mut t = [0u8; 2];
        r.read_exact(&mut t).unwrap();
        assert_eq!(&t, &want[2..4]);
        r.seek(SeekFrom::End(-3)).unwrap();
        let mut t = [0u8; 3];
        r.read_exact(&mut t).unwrap();
        assert_eq!(&t, &want[want.len() - 3..]);
        // seek 后 read 契约：EOF 返回 0
        r.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(r.read(&mut [0u8; 8]).unwrap(), 0);
    }

    /// 奇数大小文件：末尾奇字节无伙伴，保持原样交付（EOF 兜底语义）
    #[test]
    fn shift_reader_odd_size_tail_byte_passthrough() {
        let mut raw: Vec<u8> = samples().iter().flat_map(|v| v.to_le_bytes()).collect();
        let tail = 0xEE;
        raw.push(tail);
        let mut r = ShiftReader::new(Box::new(MemFile(Cursor::new(raw))), 4);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        let mut want = shifted(&samples(), 4);
        want.push(tail);
        assert_eq!(out, want);
    }
}
