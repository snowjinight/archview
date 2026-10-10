//! config.json 集中配置：加载 + 默认值写入。
//! 所有可变项（路径、扩展名、播放器参数）只在此文件配置，代码零硬编码。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub cache: CacheConfig,
    pub media: MediaConfig,
    pub player: PlayerConfig,
    pub ui: UiConfig,
    /// 密码候选代码页（WHATWG 标签，如 "gbk"/"big5"/"shift_jis"/"euc-kr"）：
    /// zip/RAR4 老工具按系统 ANSI 代码页编码密码，解压时在 UTF-8 之外追加此编码候选。
    /// "none" 或空 = 只试 UTF-8。默认 "gbk"（简中 Windows）。
    pub password_codepage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// dll 搜索路径（libmpv-2.dll），"auto" = exe 同目录
    pub dll_dir: String,
    /// 解压缓存目录（7z/RAR 慢路径），"auto" = 系统临时目录\archview_cache
    pub dir: String,
    /// 解压缓存总量上限（MB）
    pub max_size_mb: u64,
    /// 解码块大小（MB）
    pub chunk_size_mb: u64,
    /// 解码块 LRU 缓存块数
    pub chunk_cache_blocks: usize,
    /// 退出程序时自动清空解压缓存
    pub clean_on_exit: bool,
    /// 切换压缩包时自动清理上一个包的解压缓存
    pub clean_on_switch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub video_extensions: Vec<String>,
    pub image_extensions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub hwdec: String,
    pub cache_secs: u64,
    pub demuxer_max_mb: u64,
    pub volume: i64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dll_dir: "auto".into(),
            dir: "auto".into(),
            max_size_mb: 8192,
            chunk_size_mb: 4,
            chunk_cache_blocks: 64,
            clean_on_exit: false,
            clean_on_switch: false,
        }
    }
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            video_extensions: ["mp4", "mkv", "avi", "mov", "flv", "ts", "webm", "wmv", "m4v"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            image_extensions: ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            hwdec: "auto".into(),
            cache_secs: 30,
            demuxer_max_mb: 512,
            volume: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// CJK 字体文件路径；"auto" = 在系统字体目录依次尝试候选字体
    pub font_path: String,
    /// 图片自动播放（PPT 式翻页）间隔秒数
    pub slideshow_secs: f64,
    /// 视频区滚轮切换最小间隔秒数（防惯性/触控板连跳）
    pub wheel_switch_secs: f64,
    /// 翻到本包最后一个媒体项再往后导航时，是否询问打开同目录下一个压缩包
    pub ask_next_archive: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            font_path: "auto".into(),
            slideshow_secs: 1.0,
            wheel_switch_secs: 0.3,
            ask_next_archive: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache: CacheConfig::default(),
            media: MediaConfig::default(),
            player: PlayerConfig::default(),
            ui: UiConfig::default(),
            password_codepage: "gbk".into(),
        }
    }
}

impl Config {
    /// 候选位置：exe 同目录 → 当前工作目录（开发时 cargo run 的 cwd 为项目根）
    fn config_candidates() -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                v.push(d.join("config.json"));
            }
        }
        v.push(PathBuf::from("config.json"));
        v
    }

    /// 启动时读取 config.json；不存在则写入默认值。
    pub fn load_or_create() -> Result<Config, String> {
        let candidates = Self::config_candidates();
        for path in &candidates {
            if path.exists() {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("读取配置失败 {}: {e}", path.display()))?;
                return serde_json::from_str(&text).map_err(|e| format!("解析配置失败: {e}"));
            }
        }
        let cfg = Config::default();
        let path = &candidates[0];
        let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("写入配置失败 {}: {e}", path.display()))?;
        Ok(cfg)
    }

    pub fn is_video_ext(&self, name: &str) -> bool {
        Self::has_ext(&self.media.video_extensions, name)
    }

    pub fn is_image_ext(&self, name: &str) -> bool {
        Self::has_ext(&self.media.image_extensions, name)
    }

    fn has_ext(list: &[String], name: &str) -> bool {
        let ext = name.rsplit('.').next().unwrap_or("");
        list.iter().any(|e| e.eq_ignore_ascii_case(ext))
    }

    /// CJK 字体文件路径：显式路径直接用；"auto" 在系统字体目录按候选顺序找
    pub fn cjk_font_path(&self) -> Option<PathBuf> {
        if self.ui.font_path != "auto" {
            let p = PathBuf::from(&self.ui.font_path);
            return p.exists().then_some(p);
        }
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let fonts_dir = PathBuf::from(windir).join("Fonts");
        ["msyh.ttc", "msyhdb.ttc", "simhei.ttf", "simsun.ttc"]
            .iter()
            .map(|f| fonts_dir.join(f))
            .find(|p| p.exists())
    }

    /// libmpv-2.dll 所在目录（"auto" = exe 同目录，即 deps\mpv-dev 也可配绝对路径）
    pub fn dll_path(&self) -> PathBuf {
        if self.cache.dll_dir == "auto" {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_default()
        } else {
            PathBuf::from(&self.cache.dll_dir)
        }
    }
}
