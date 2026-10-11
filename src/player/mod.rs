//! 播放服务：持有 mpv 实例，封装命令/属性访问。
//! UI 线程专用（mpv handle 本身线程安全，但回调线程不得触碰，见 stream_cb.rs）。

pub mod mpv_ffi;
pub mod stream_cb;

use crate::config::Config;

pub struct PlayerService {
    ctx: *mut mpv_ffi::MpvHandle,
}

// mpv client API 线程安全；回调线程只走全局注册表，不触碰此 handle。
unsafe impl Send for PlayerService {}

impl PlayerService {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let ctx = mpv_ffi::create()?;
        let s = Self { ctx };
        // 播放器参数全部来自 config.json
        s.opt("hwdec", &cfg.player.hwdec);
        s.opt("cache-secs", &cfg.player.cache_secs.to_string());
        s.opt(
            "demuxer-max-bytes",
            &format!("{}MiB", cfg.player.demuxer_max_mb),
        );
        s.opt("keep-open", "always"); // 播完停在末帧，不触发退出
        s.opt("osc", "no"); // 禁用 mpv 内置 OSC（预览窗口里的白条进度条），进度条由 UI 提供
        s.opt("osd-level", "0"); // 不在预览窗口里弹 OSD 提示
        s.set_prop_str("volume", &cfg.player.volume.to_string());
        // 核心：注册 zvf:// 自定义协议
        mpv_ffi::stream_cb_add_ro(ctx, "zvf", std::ptr::null_mut(), stream_cb::open_cb)
            .map_err(|e| format!("注册 zvf:// 协议失败: {e}"))?;
        Ok(s)
    }

    fn opt(&self, name: &str, value: &str) {
        if let Err(e) = mpv_ffi::set_option_string(self.ctx, name, value) {
            tracing::warn!("mpv 选项 {name}={value} 设置失败: {e}");
        }
    }

    fn set_prop_str(&self, name: &str, value: &str) {
        if let Err(e) = mpv_ffi::set_property_string(self.ctx, name, value) {
            tracing::warn!("mpv 属性 {name}={value} 设置失败: {e}");
        }
    }

    /// 嵌入原生子窗口（Windows：HWND）
    pub fn embed(&self, hwnd: isize) {
        if let Err(e) = mpv_ffi::set_option_string(self.ctx, "wid", &hwnd.to_string()) {
            tracing::error!("设置 wid 失败: {e}");
        }
    }

    pub fn load(&self, archive_id: u64, entry_idx: usize) {
        let uri = stream_cb::entry_uri(archive_id, entry_idx);
        if let Err(e) = mpv_ffi::command_string(self.ctx, &format!("loadfile {uri} replace")) {
            tracing::error!("loadfile 失败: {e}");
        }
        // keep-open=always 在 EOF 时会把 pause 置为 yes，且 loadfile 不清除它——
        // 不显式取消暂停的话，播完一个视频后切换新视频会停在第一帧
        self.set_prop_str("pause", "no");
    }

    /// 播放本地文件路径（7z/RAR 后台解压完成后的慢路径）
    pub fn load_path(&self, path: &std::path::Path) {
        let url = to_file_url(path);
        if let Err(e) = mpv_ffi::command_string(self.ctx, &format!("loadfile {url} replace")) {
            tracing::error!("loadfile {url} 失败: {e}");
        }
        self.set_prop_str("pause", "no"); // 同 load()：清除 EOF 粘滞的暂停态
    }

    pub fn toggle_pause(&self) {
        // pause 是 flag 类型属性，必须用 MPV_FORMAT_FLAG 读；
        // 用 INT64 读会失败（mpv 不做跨格式转换），导致永远读到"未暂停"、恢复不了播放
        let paused = mpv_ffi::get_property_flag(self.ctx, "pause")
            .ok()
            .flatten()
            .unwrap_or(false);
        self.set_prop_str("pause", if paused { "no" } else { "yes" });
    }

    pub fn seek(&self, sec: f64) {
        // no-osd 前缀：不在预览窗口里闪现 mpv 的 OSD 进度条
        if let Err(e) = mpv_ffi::command_string(self.ctx, &format!("no-osd seek {sec} absolute")) {
            tracing::error!("seek 失败: {e}");
        }
    }

    /// 相对 seek（秒，可为负），mpv 默认关键帧级（快）
    pub fn seek_relative(&self, sec: f64) {
        if let Err(e) = mpv_ffi::command_string(self.ctx, &format!("no-osd seek {sec:+} relative"))
        {
            tracing::error!("seek 相对跳转失败: {e}");
        }
    }

    /// 静音切换
    pub fn toggle_mute(&self) {
        let muted = mpv_ffi::get_property_flag(self.ctx, "mute")
            .ok()
            .flatten()
            .unwrap_or(false);
        self.set_prop_str("mute", if muted { "no" } else { "yes" });
    }

    /// 当前是否静音
    pub fn muted(&self) -> bool {
        mpv_ffi::get_property_flag(self.ctx, "mute")
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    pub fn set_volume(&self, v: i64) {
        self.set_prop_str("volume", &v.to_string());
        // 主动调整音量即解除静音（含拖动音量条和方向键调音量）
        self.set_prop_str("mute", "no");
    }

    pub fn time_pos(&self) -> Option<f64> {
        mpv_ffi::get_property_f64(self.ctx, "time-pos")
            .ok()
            .flatten()
    }

    pub fn duration(&self) -> Option<f64> {
        mpv_ffi::get_property_f64(self.ctx, "duration")
            .ok()
            .flatten()
    }

    pub fn paused(&self) -> bool {
        mpv_ffi::get_property_flag(self.ctx, "pause")
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    pub fn buffering(&self) -> bool {
        mpv_ffi::get_property_flag(self.ctx, "pause-for-cache")
            .ok()
            .flatten()
            .unwrap_or(false)
    }
}

impl Drop for PlayerService {
    fn drop(&mut self) {
        mpv_ffi::destroy(self.ctx);
    }
}

/// Windows 路径 → file:// URL（百分号编码，避免 mpv 命令解析吃掉空格/反斜杠）
fn to_file_url(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! 自动化端到端播放测试：无需 UI/人工，模拟真实用户播放。
    //! headless mpv（vo=null ao=null）+ cache=no（强制每次 seek 打到 VFS 层），
    //! 覆盖：mpv FFI 加载、zvf:// stream_cb open/read/seek/close、deflate seek 适配器。
    //! 前置：libmpv-2.dll（build.rs 已复制到 target）+ ffmpeg 生成测试视频，缺任一则跳过。

    use super::*;
    use crate::vfs::zip_vfs::ZipVFS;
    use crate::vfs::ArchiveVFS;
    use std::path::PathBuf;

    /// 查找 libmpv-2.dll 所在目录：test exe 在 target\{debug,release}\deps\ 下，
    /// dll 在上一级；也兼容直接 target\{debug,release}\ 与 CARGO_MANIFEST_DIR。
    fn find_mpv_dll_dir() -> Option<PathBuf> {
        let mut cands: Vec<PathBuf> = vec![];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                cands.push(d.to_path_buf());
                if let Some(dd) = d.parent() {
                    cands.push(dd.to_path_buf());
                }
            }
        }
        if let Ok(m) = std::env::var("CARGO_MANIFEST_DIR") {
            for prof in ["debug", "release"] {
                cands.push(PathBuf::from(&m).join("target").join(prof));
            }
        }
        cands.into_iter().find(|d| d.join("libmpv-2.dll").exists())
    }

    /// ffmpeg 是否可用
    fn have_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// 生成 3 秒测试视频并打进 deflate zip（两个 entry），返回 zip 路径
    fn make_test_zip() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("zvf_e2e_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let mp4 = dir.join("clip.mp4");
        // 3 秒 320x180 testsrc2，超快速编码
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=3:size=320x180:rate=15",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mp4)
            .output()
            .ok()?;
        if !st.status.success() {
            eprintln!("ffmpeg 生成失败: {}", String::from_utf8_lossy(&st.stderr));
            return None;
        }
        let data = std::fs::read(&mp4).ok()?;
        let zip_path = dir.join("test.zip");
        let f = std::fs::File::create(&zip_path).ok()?;
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("clip.mp4", opts).ok()?;
        std::io::Write::write_all(&mut w, &data).ok()?;
        // 第二个 entry：用于"播完后切换视频"场景
        w.start_file("clip2.mp4", opts).ok()?;
        std::io::Write::write_all(&mut w, &data).ok()?;
        w.finish().ok()?;
        Some(zip_path)
    }

    /// 轮询直到属性满足条件，超时返回 None
    fn wait_prop(
        ctx: *mut mpv_ffi::MpvHandle,
        name: &str,
        pred: impl Fn(f64) -> bool,
        secs: u64,
    ) -> Option<f64> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            if let Ok(Some(v)) = mpv_ffi::get_property_f64(ctx, name) {
                if pred(v) {
                    return Some(v);
                }
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// 端到端：zip 内 deflate 视频 → zvf:// 流 → headless mpv 播放 + 前向/后向 seek
    #[test]
    fn e2e_playback_with_seek_via_stream_cb() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        if !have_ffmpeg() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }
        let Some(zip_path) = make_test_zip() else {
            eprintln!("跳过：测试 zip 生成失败");
            return;
        };

        mpv_ffi::init(&dll_dir).expect("加载 libmpv 失败");

        // 注册 VFS
        let vfs = ZipVFS::open(&zip_path, None).unwrap();
        let arc: std::sync::Arc<dyn crate::vfs::ArchiveVFS> = std::sync::Arc::new(vfs);
        let (archive_id, entries) = stream_cb::register_with_progress(arc, &zip_path, &|_, _| {});
        let entry_idx = entries
            .iter()
            .position(|e| e.name == "clip.mp4")
            .expect("zip 内应有 clip.mp4");

        // headless mpv：无窗口渲染；cache=no 强制 seek 直接打到 VFS；
        // keep-open=always 与 App 的 PlayerService 一致（复现播完暂停场景）
        let ctx = mpv_ffi::create().unwrap();
        mpv_ffi::set_option_string(ctx, "terminal", "yes").unwrap();
        mpv_ffi::set_option_string(ctx, "msg-level", "all=error").unwrap();
        mpv_ffi::set_option_string(ctx, "vo", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "ao", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "cache", "no").unwrap();
        mpv_ffi::set_option_string(ctx, "keep-open", "always").unwrap();
        mpv_ffi::stream_cb_add_ro(ctx, "zvf", std::ptr::null_mut(), stream_cb::open_cb)
            .expect("注册 zvf:// 失败");

        // 播放
        let uri = stream_cb::entry_uri(archive_id, entry_idx);
        mpv_ffi::command_string(ctx, &format!("loadfile {uri}")).expect("loadfile 失败");

        // 1) 起播：time-pos > 0.5s（vo=null 全速解码，瞬间到达）
        let started = wait_prop(ctx, "time-pos", |t| t > 0.5, 30);
        assert!(
            started.is_some(),
            "30 秒内未起播（stream_cb 链路可能崩溃/挂起）"
        );

        // 1.1) pause 属性必须用 flag 格式读：toggle_pause/paused 依赖它。
        // 此前用 INT64 读恒失败 → toggle_pause 永远认为"未暂停" → 空格/暂停按钮无法恢复播放
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            Some(false),
            "flag 方式应能读到 pause=no"
        );
        mpv_ffi::set_property_string(ctx, "pause", "yes").unwrap();
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            Some(true),
            "置 yes 后 flag 方式应能读到 pause=yes"
        );
        // toggle 循环（暂停按钮/空格的完整行为路径）：yes → 翻转回 no
        mpv_ffi::set_property_string(ctx, "pause", "no").unwrap();
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            Some(false),
            "toggle 回 no 后应能读到 pause=no"
        );
        // mute 同为 flag 属性（静音按钮路径）
        mpv_ffi::set_property_string(ctx, "mute", "yes").unwrap();
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "mute").ok().flatten(),
            Some(true),
            "mute 置 yes 后 flag 方式应能读到"
        );
        mpv_ffi::set_property_string(ctx, "mute", "no").unwrap();

        // 2) 时长合理（3s ± 1）
        let dur = mpv_ffi::get_property_f64(ctx, "duration")
            .ok()
            .flatten()
            .expect("读不到 duration");
        assert!((dur - 3.0).abs() < 1.0, "duration 异常: {dur}");

        // 暂停：time-pos 冻结，seek 验证不再受全速播放影响（否则轮询可能错过断言窗口）
        mpv_ffi::set_property_string(ctx, "pause", "yes").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 3) 前向 seek 到中点
        let target = dur * 0.5;
        mpv_ffi::command_string(ctx, &format!("seek {target} absolute")).unwrap();
        let reached = wait_prop(ctx, "time-pos", |t| (t - target).abs() < 1.0, 30);
        assert!(reached.is_some(), "前向 seek 超时");

        // 4) 后向 seek 回 15% 处（deflate 重解压路径）
        let back = dur * 0.15;
        mpv_ffi::command_string(ctx, &format!("seek {back} absolute")).unwrap();
        let reached_back = wait_prop(ctx, "time-pos", |t| (t - back).abs() < 1.0, 30);
        assert!(reached_back.is_some(), "后向 seek 超时");

        // 5) 真实场景复现：播完第一个视频（EOF）后切换第二个视频
        // 取消暂停让视频自然播到结尾（vo=null 全速），等待真正的 eof-reached=yes
        // （keep-open=always 下 mpv 会在结尾暂停——正是用户遇到的场景）
        mpv_ffi::set_property_string(ctx, "pause", "no").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let eof = mpv_ffi::get_property_flag(ctx, "eof-reached")
                .ok()
                .flatten();
            if eof == Some(true) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "未到达 EOF（eof-reached={eof:?}, time-pos={:?}, pause={:?}）",
                mpv_ffi::get_property_f64(ctx, "time-pos").ok().flatten(),
                mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let pause_at_eof = mpv_ffi::get_property_flag(ctx, "pause").ok().flatten();
        eprintln!("EOF 到达，此时 pause = {pause_at_eof:?}");

        // 切换第二个视频（同一压缩包不同 entry，与 UI 点击行为一致）
        let entry_idx2 = entries
            .iter()
            .position(|e| e.name == "clip2.mp4")
            .expect("zip 内应有 clip2.mp4");
        let uri2 = stream_cb::entry_uri(archive_id, entry_idx2);
        mpv_ffi::command_string(ctx, &format!("loadfile {uri2} replace")).expect("loadfile 失败");
        // 修复验证：loadfile 后显式取消暂停（PlayerService::load 的行为）
        mpv_ffi::set_property_string(ctx, "pause", "no").unwrap();
        let started2 = wait_prop(ctx, "time-pos", |t| t > 0.5, 15);
        let pause_after_load = mpv_ffi::get_property_flag(ctx, "pause").ok().flatten();
        eprintln!("切换后 time-pos = {started2:?}, pause = {pause_after_load:?}");
        assert!(
            started2.is_some(),
            "播完 EOF 后切换第二个视频未起播（pause={pause_after_load:?}：keep-open EOF 暂停跨 loadfile 粘滞，需显式 pause=no）"
        );

        mpv_ffi::destroy(ctx);
        let _ = std::fs::remove_dir_all(zip_path.parent().unwrap());
        eprintln!("端到端播放+seek 测试通过 ✓");
    }

    /// 端到端（慢路径）：7z 包内视频 → SevenzVFS 后台解压 → headless mpv 播本地文件 + seek。
    /// 7z 测试包用 sevenz-rust writer 纯 Rust 自造，无需外部 7z.exe。
    #[test]
    fn e2e_playback_7z_extract_then_play() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        if !have_ffmpeg() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }

        // 生成 3 秒测试视频
        let dir = std::env::temp_dir().join(format!("zvf_7z_e2e_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("clip.mp4");
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=3:size=320x180:rate=15",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mp4)
            .output()
            .unwrap();
        assert!(st.status.success(), "ffmpeg 生成失败");

        // 纯 Rust 打 7z 包（LZMA2）
        let arch_path = dir.join("test.7z");
        let mut w = sevenz_rust::SevenZWriter::create(&arch_path).unwrap();
        let entry = sevenz_rust::SevenZArchiveEntry::from_path(&mp4, "movies/clip.mp4".into());
        w.push_archive_entry(entry, Some(std::fs::File::open(&mp4).unwrap()))
            .unwrap();
        w.finish().unwrap();

        // VFS 解压（慢路径）
        let vfs = crate::vfs::sevenz_vfs::SevenzVFS::open(&arch_path, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "movies/clip.mp4");
        let extracted = dir.join("extracted.mp4");
        vfs.extract_entry(
            &entries[0],
            &extracted,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&extracted).unwrap().len(),
            entries[0].size
        );

        // headless mpv 播放解压后的本地文件
        mpv_ffi::init(&dll_dir).expect("加载 libmpv 失败");
        let ctx = mpv_ffi::create().unwrap();
        mpv_ffi::set_option_string(ctx, "vo", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "ao", "null").unwrap();
        let url = to_file_url(&extracted);
        mpv_ffi::command_string(ctx, &format!("loadfile {url}")).unwrap();

        let started = wait_prop(ctx, "time-pos", |t| t > 0.5, 30);
        assert!(started.is_some(), "7z 解压路径未起播");
        let dur = mpv_ffi::get_property_f64(ctx, "duration")
            .ok()
            .flatten()
            .unwrap();
        assert!((dur - 3.0).abs() < 1.0, "duration 异常: {dur}");

        mpv_ffi::set_property_string(ctx, "pause", "yes").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let target = dur * 0.5;
        mpv_ffi::command_string(ctx, &format!("seek {target} absolute")).unwrap();
        let reached = wait_prop(ctx, "time-pos", |t| (t - target).abs() < 1.0, 30);
        assert!(reached.is_some(), "7z 路径 seek 超时");

        mpv_ffi::destroy(ctx);
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("7z 解压播放端到端测试通过 ✓");
    }
}
