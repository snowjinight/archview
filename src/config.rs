//! config.toml 集中配置：加载 + 默认值写入。
//! 所有可变项（路径、扩展名、播放器参数）只在此文件配置，代码零硬编码。
//! 默认文件由手写模板 DEFAULT_TOML 生成（带中文注释）；模板必须与
//! Config::default() 逐项一致（防漂移测试 default_template_matches_config_default 钉死）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use toml_edit::{DocumentMut, Table};

const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub cache: CacheConfig,
    pub media: MediaConfig,
    pub player: PlayerConfig,
    pub ui: UiConfig,
    pub extract: ExtractConfig,
    /// 非 UTF-8 字节兜底代码页（WHATWG 标签，如 "gbk"/"big5"/"shift_jis"/"euc-kr"）：
    /// 老工具（老 WinRAR/好压等）按系统 ANSI 代码页打包，密码与 zip 文件名同源编码。
    /// 此编码用于：密码候选追加（UTF-8 之外再试）+ zip 文件名解码兜底
    /// （无 UTF-8 标志且非合法 UTF-8 的名字，中文包即 GBK）。
    /// "none" 或空 = 只用 UTF-8。默认 "gbk"（简中 Windows）。
    pub codepage: String,
    /// 加密包密码候选列表（播放器与 VarTidy 公用，按序尝试）。
    /// 打开压缩包阶段即自动探测（无密码优先 → 此列表依次），全不匹配播放器才弹
    /// 密码框、VarTidy 记失败；CLI 的 -p 密码排在此列表之前。
    /// 密码仅用于解压，不写入日志/报告
    pub passwords: Vec<String>,
    /// VarTidy（`archview tidy` CLI 子命令）配置
    pub vartidy: VarTidyConfig,
    /// 主程序日志级别：error/warn/info/debug/trace，也支持 EnvFilter 指令
    /// 语法（如 "info,archview::ui::panels=debug" 只开某模块 debug 诊断）。
    /// RUST_LOG 环境变量优先生效
    pub log_level: String,
}

/// VarTidy 配置：仓库与报告路径集中此处，代码零硬编码（ARCHITECTURE.md 12.6）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VarTidyConfig {
    /// var 仓库根目录。必须已存在——VarTidy 绝不创建/修改/删除仓库中已有内容，
    /// 仅在仓库中新建作者子目录并写入仓库没有的新 var。空 = 未配置（CLI 需 -o 指定）
    pub repo_dir: String,
    /// 报告输出目录；"auto" = 首个输入压缩包所在目录（不写入仓库，保持仓库纯净）
    pub report_dir: String,
}

impl Default for VarTidyConfig {
    fn default() -> Self {
        Self {
            repo_dir: String::new(),
            report_dir: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractConfig {
    /// 下游预取上限（MB），0 = 关闭：solid 包（7z/solid RAR）解压时顺带预取；
    /// 非 solid RAR 走单文件按需，不组批，此项不生效
    /// 后续文件（下标更大的方向 = 列表向下 = 解码流推进方向）的总大小上限。
    /// 一次顺序解压流解出一批文件，翻页时直接命中缓存，摊薄"定位目标需
    /// 顺序解压前置数据"的固定成本
    #[serde(alias = "prefetch_mb")]
    pub prefetch_downstream_mb: u64,
    /// 上游预取上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置
    /// 条目（下标更小的方向 = 列表向上；7z 限于目标所在 folder 内、RAR
    /// 为整个前缀）顺带写盘，随机跳看后回看直接命中缓存。
    /// 解码零增量，仅占磁盘写入量
    #[serde(alias = "prefetch_prev_mb")]
    pub prefetch_upstream_mb: u64,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            prefetch_downstream_mb: 512,
            prefetch_upstream_mb: 512,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub video_extensions: Vec<String>,
    pub image_extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub hwdec: String,
    pub cache_secs: u64,
    pub demuxer_max_mb: u64,
    pub volume: i64,
    /// 订阅 mpv 内部日志（"no" 关闭；诊断切换延迟时可设 "v"）
    pub mpv_log_level: String,
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
            video_extensions: [
                "mp4", "mkv", "avi", "mov", "flv", "ts", "webm", "wmv", "m4v",
            ]
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
            mpv_log_level: "no".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 文件列表是否按自然序排序（数字段按数值：1/2/10；不区分大小写）。
    /// false（默认）= 保持压缩包内部顺序：7z/RAR 固态包下内部顺序 == 物理码流顺序，
    /// 预取直接命中物理相邻文件（性能最优）。注意 7-Zip 工具打的包内部本来就是
    /// 字典序，关闭排序对这类包显示无变化。
    /// true = 自然序：看漫画/图集时开启，编号文件按人类预期排列（1/2/10）
    pub sort_files: bool,
    /// 解压覆盖层显示前的宽限期（毫秒）：正在播放视频时切换到需解压的条目，
    /// 超过该时长仍未解完才隐藏视频窗口并显示进度覆盖层——快速解压
    /// （缓存命中/小文件）全程不打扰播放，避免窗口闪烁
    pub extract_overlay_delay_ms: u64,
    /// 图片 egui 纹理直显（后台 image crate 解码 → 纹理，跳过 mpv；静图
    /// 与动图 gif 均走此链路，解码失败自动回退 mpv）
    pub image_texture: bool,
    /// 图片纹理 LRU 缓存张数（含当前张）：±1 相邻页预解码，翻页零等待
    pub image_cache_count: usize,
    /// 基础纹理档尺寸上限：预放大（×ppp）后超过则原尺寸直进；full 高清
    /// 升级资格判据同此值（见 imgview.rs resample 两档策略）
    pub image_max_dim: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            font_path: "auto".into(),
            slideshow_secs: 1.0,
            wheel_switch_secs: 0.3,
            ask_next_archive: true,
            sort_files: false,
            extract_overlay_delay_ms: 500,
            image_texture: true,
            image_cache_count: 3,
            image_max_dim: 4096,
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
            extract: ExtractConfig::default(),
            codepage: "gbk".into(),
            passwords: Vec::new(),
            vartidy: VarTidyConfig::default(),
            log_level: "info".into(),
        }
    }
}

impl Config {
    /// 候选位置：exe 同目录 → 当前工作目录（开发时 cargo run 的 cwd 为项目根）
    fn config_candidates() -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                v.push(d.join(CONFIG_FILE));
            }
        }
        v.push(PathBuf::from(CONFIG_FILE));
        v
    }

    /// 启动时读取 config.toml；不存在则写入带中文注释的默认模板
    pub fn load_or_create() -> Result<Config, String> {
        let candidates = Self::config_candidates();
        for path in &candidates {
            if path.exists() {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("读取配置失败 {}: {e}", path.display()))?;
                let cfg: Config = toml::from_str(&text).map_err(|e| {
                    format!(
                        "解析配置失败 {}: {e}\n（TOML：字符串必须双引号；路径写正斜杠 D:/dir 或双反斜杠 D:\\\\dir）",
                        path.display()
                    )
                })?;
                Self::backfill(path, &text);
                return Ok(cfg);
            }
        }
        let cfg = Config::default();
        let dst = &candidates[0];
        // trim_start：模板字面量 r#" 后的换行只为源码排版，生成文件不带首行空行
        std::fs::write(dst, DEFAULT_TOML.trim_start())
            .map_err(|e| format!("写入配置失败 {}: {e}", dst.display()))?;
        Ok(cfg)
    }

    /// 用户 config 缺失键补全：以默认模板为底档，把用户文档缺的键（含注释）
    /// 补上并写回。已有键值/注释/键序原样保留（toml_edit 逐项编辑保留格式）。
    /// 失败静默——本次启动配置已经 serde(default) 兜底默认值，
    /// 补全只影响文件内容（便于用户下次查看/手动编辑）
    fn backfill(path: &std::path::Path, user_text: &str) {
        let Ok(mut doc) = user_text.parse::<DocumentMut>() else { return };
        let Ok(def) = DEFAULT_TOML.parse::<DocumentMut>() else { return };
        if backfill_table(doc.as_table_mut(), def.as_table()) > 0 {
            let _ = std::fs::write(path, doc.to_string());
        }
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

    /// CJK 字体文件路径列表：显式路径直接用（单个）；"auto" 收集系统字体目录里
    /// 中/日/韩三组候选字体（egui 同族内按序尝试字形，全部加载覆盖 zh/ja/ko）
    pub fn cjk_font_paths(&self) -> Vec<PathBuf> {
        if self.ui.font_path != "auto" {
            let p = PathBuf::from(&self.ui.font_path);
            return p.exists().then_some(p).into_iter().collect();
        }
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let fonts_dir = PathBuf::from(windir).join("Fonts");
        [
            // 中文（微软雅黑/黑体/宋体）
            "msyh.ttc",
            "msyhdb.ttc",
            "simhei.ttf",
            "simsun.ttc",
            // 日文（游ゴシック Win8.1+ / Meiryo / MS Gothic）：
            // 雅黑对假名覆盖不全，日文字体补齐假名与 JIS 汉字
            "YuGothM.ttc",
            "meiryo.ttc",
            "msgothic.ttc",
            // 韩文（맑은 고딕）：雅黑/日文字体均不含谚文，必须单独补
            "malgun.ttf",
        ]
        .iter()
        .map(|f| fonts_dir.join(f))
        .filter(|p| p.exists())
        .collect()
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

/// 递归把 src（默认模板）里 dst（用户文档）缺失的键补进 dst，返回补全项数。
/// 克隆的 Item 自带前置注释；已有键一律不动（用户的值和注释优先）；
/// 数组不逐项合并（用户存在即整用用户的）
fn backfill_table(dst: &mut Table, src: &Table) -> usize {
    let mut n = 0;
    for (key, src_item) in src.iter() {
        if !dst.contains_key(key) {
            dst.insert(key, src_item.clone());
            n += 1;
        } else if let (Some(st), Some(dt)) = (
            src_item.as_table(),
            dst.get_mut(key).and_then(|i| i.as_table_mut()),
        ) {
            // 双方都是子表：递归补子键（根级新叶子键即使 append 到文档末尾，
            // toml_edit 序列化时叶子先于子表输出，仍会出现在第一个 [表] 之前）
            n += backfill_table(dt, st);
        }
    }
    n
}

/// 默认配置模板（带中文注释，写入全新安装的 config.toml）。
/// 约束：解析结果必须与 Config::default() 完全一致——
/// 防漂移测试 default_template_matches_config_default 钉死，改默认值须同步改模板。
const DEFAULT_TOML: &str = r#"
	# =============================================================
	# Archview 配置文件（TOML）
	# 启动时按「exe 同目录 → 当前工作目录」查找 config.toml，不存在则生成本文件。
	# 已有配置缺新版本新增的键时，启动自动按默认值补全并写回
	# （已有键值与注释原样保留）。
	# 通用规则：
	#   * 字符串一律用双引号；含 # 、空格等特殊字符的值（尤其密码）更必须双引号
	#   * 路径推荐正斜杠 "D:/dir"；反斜杠必须双写 "D:\\dir"（TOML 转义）
	#   * "auto" 表示自动探测；相对路径相对启动时工作目录，建议绝对路径
	# =============================================================

	# 非 UTF-8 字节兜底代码页：老工具（老 WinRAR/好压等）按系统 ANSI 代码页打包，
	# 密码与 zip 文件名同源编码。用于密码候选追加 + zip 文件名解码兜底
	# （无 UTF-8 标志且非合法 UTF-8 的名字，中文包即 GBK）。
	# 取值如 "gbk" / "big5" / "shift_jis" / "euc-kr"；"none" = 只用 UTF-8
codepage = "gbk"

	# 加密包密码候选列表（播放器与 VarTidy 公用，按序尝试）。
	# 打开压缩包阶段即自动探测：无密码优先 → 本列表依次；全不匹配时播放器才弹
	# 密码框、VarTidy 记失败跳包。密码始终用双引号包裹；仅用于解压，不写入日志/报告
passwords = []

	# 主程序日志级别：error / warn / info / debug / trace。
	# 也支持按模块指令（EnvFilter 语法）只开某模块的诊断日志，如帧节奏探针：
	#   log_level = "info,archview::ui::panels=debug"
	# RUST_LOG 环境变量优先生效
log_level = "info"

[cache]
	# libmpv-2.dll 搜索目录；"auto" = exe 同目录
dll_dir = "auto"
	# 7Z/RAR 解压缓存目录；"auto" = %TEMP%\archview_cache
	# 可指向 RAM Disk 提速（见 README「RAM Disk 缓存」）；目录不存在会自动创建
dir = "auto"
	# 缓存总量上限（MB），超出按 LRU（最久未访问）驱逐
max_size_mb = 8192
	# 解码块大小（MB）
chunk_size_mb = 4
	# 解码块 LRU 缓存块数
chunk_cache_blocks = 64
	# 退出程序时自动清空解压缓存
clean_on_exit = false
	# 切换压缩包时自动清理上一个包的解压缓存（false = 保留以便切回命中）
clean_on_switch = false

[media]
	# 视频 / 图片的文件扩展名识别列表（不区分大小写）
video_extensions = ["mp4", "mkv", "avi", "mov", "flv", "ts", "webm", "wmv", "m4v"]
image_extensions = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"]

[player]
	# 硬件解码："auto" / "no" / "d3d11va" 等 mpv 取值
hwdec = "auto"
	# mpv 缓存秒数（拖回刚看过的片段直接命中缓存，零成本）
cache_secs = 30
	# mpv demuxer 最大缓存（MB）
demuxer_max_mb = 512
	# 初始音量（0-100）
volume = 100
	# mpv 内部日志级别（no/fatal/error/warn/info/status/v/debug/trace）。
	# 诊断 mpv 内部行为（demux 探测/hwdec/解码链路）时临时设 "v"——
	# 量极大，转发到主程序日志；平时保持 "no"
mpv_log_level = "no"

[ui]
	# CJK 字体路径；"auto" = 自动加载系统中/日/韩候选字体链（支持中日韩文件名显示）
	# 也可指定单个字体文件的绝对路径
font_path = "auto"
	# 图片自动播放（PPT 式翻页）间隔秒数
slideshow_secs = 1.0
	# 视频区滚轮切换媒体的最小间隔秒数（防惯性/触控板连跳）
wheel_switch_secs = 0.3
	# 翻到本包最后一项再往后导航时，是否弹窗选择同目录其它压缩包
ask_next_archive = true
	# 文件列表是否按自然序排序（数字段按数值：1/2/10；不区分大小写）。
	# false（默认）= 保持压缩包内部顺序：固态包下内部顺序 == 物理码流顺序，
	#   预取直接命中物理相邻文件（翻页性能最优）。
	#   注意：7-Zip 工具打的包内部本来就是字典序，关闭排序对这类包显示无变化。
	# true = 自然序：看漫画/图集时开启，编号文件按人类预期排列（1/2/10，
	#   而非字典序的 1/10/2）
sort_files = false
	# 解压进度覆盖层显示前的宽限期（毫秒）：播放中切换到需解压的条目，
	# 超过该时长仍未解完才隐藏视频窗口显示进度；快速解压全程不打扰播放
extract_overlay_delay_ms = 500
	# 图片 egui 纹理直显（后台解码直接上纹理，翻页秒开；静图与动图 gif 均
	# 走此链路）。解码失败的图片自动回退 mpv
image_texture = true
	# 图片纹理 LRU 缓存张数（含当前张）：预解码 ±1 相邻页，翻页零等待
image_cache_count = 3
	# 基础纹理档尺寸上限：预放大（×ppp）后超过则原尺寸直进；full 高清
	# 升级资格判据同此值（详见 IMGVIEW_HANDOVER.md 两档策略）
image_max_dim = 4096

[extract]
	# 下游预取上限（MB），0 = 关闭：solid 包（7z/solid RAR）解压时顺带预取后续文件
	# （下标更大 = 列表向下 = 解码流推进方向）。一次顺序解压流解出一批文件，
	# 翻页直接命中，摊薄固态包定位成本
prefetch_downstream_mb = 512
	# 上游预取上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置条目
	# （下标更小 = 列表向上；7z 限于目标所在 folder 内、RAR 为整个前缀）顺带
	# 写盘，随机跳看后回看直接命中缓存。解码零增量，仅占磁盘写入量
	# （旧键名 prefetch_mb / prefetch_prev_mb 仍可读取）
prefetch_upstream_mb = 512

[vartidy]
	# VarTidy（"archview.exe tidy" CLI 子命令）配置
	# var 仓库根目录。必须已存在——整理器绝不修改/移动/删除仓库已有内容，
	# 仅新建作者子目录并写入仓库没有的新 var。空 = 未配置（CLI 需 -o 指定）
repo_dir = ""
	# 报告输出目录；"auto" = 首个输入压缩包所在目录（不写入仓库，保持仓库纯净）
report_dir = "auto"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// 防漂移：带注释的默认模板解析回来必须与代码默认值逐项一致。
    /// 任何一侧改默认值都必须同步另一侧，本测试会拦住遗忘
    #[test]
    fn default_template_matches_config_default() {
        let from_tpl: Config = toml::from_str(DEFAULT_TOML).expect("默认模板必须是合法 TOML");
        assert_eq!(from_tpl, Config::default());
    }

    /// 序列化往返：结构体直接序列化的 TOML（无注释）可原样读回
    #[test]
    fn toml_roundtrip() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, Config::default());
    }

    /// 旧键名兼容：prefetch_mb / prefetch_prev_mb（上游/下游正名前）经
    /// serde alias 仍可解析，老用户的 config.toml 不失效
    #[test]
    fn extract_prefetch_legacy_keys() {
        let cfg: Config = toml::from_str(
            "[extract]
prefetch_mb = 7
prefetch_prev_mb = 9",
        )
        .unwrap();
        assert_eq!(cfg.extract.prefetch_downstream_mb, 7);
        assert_eq!(cfg.extract.prefetch_upstream_mb, 9);
    }

    /// 含特殊字符（#、空格、中文）的密码在 TOML 双引号内原样保留
    #[test]
    fn toml_special_char_passwords() {
        let text = r#"
codepage = "gbk"
passwords = ["p@ss #1", "中文密码 2026", "特!@#$%^&*()殊"]
[vartidy]
repo_dir = ""
report_dir = "auto"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(
            cfg.passwords,
            ["p@ss #1", "中文密码 2026", "特!@#$%^&*()殊"]
        );
    }

    /// 缺失键补全：用户 config 缺根级键与 [ui] 子键时从默认模板补齐（带注释）；
    /// 已有键值/注释原样保留；补全结果是合法 TOML 且根级新键落在第一个
    /// [表] 之前（不被错误归属到末尾的表）
    #[test]
    fn backfill_missing_keys() {
        let user = r#"
# 我的配置
codepage = "big5"   # 用户注释保留

[ui]
slideshow_secs = 2.0
"#;
        let mut doc: DocumentMut = user.parse().unwrap();
        let def: DocumentMut = DEFAULT_TOML.parse().unwrap();
        assert!(backfill_table(doc.as_table_mut(), def.as_table()) > 0);
        let out = doc.to_string();
        // 已有键的值与注释原样保留
        assert!(out.contains("# 我的配置"));
        assert!(out.contains("codepage = \"big5\"   # 用户注释保留"));
        assert!(out.contains("slideshow_secs = 2.0"));
        // 缺失键按默认值补上
        assert!(out.contains("log_level = \"info\""));
        assert!(out.contains("image_max_dim = 4096"));
        // 根级新键必须在第一个 [表] 头之前（否则会归属到最后一个表）
        let key_pos = out.find("log_level").expect("log_level 已补全");
        let tbl_pos = out.find("\n[").expect("存在表头");
        assert!(key_pos < tbl_pos, "根级补全键应出现在首个表头之前");
        // 补全结果语义正确：用户值优先 + 缺失项取默认
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.codepage, "big5");
        assert_eq!(cfg.ui.slideshow_secs, 2.0);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.ui.image_max_dim, 4096);
        // 幂等：再跑一遍补全 0 项，输出不变
        let mut doc2: DocumentMut = out.parse().unwrap();
        assert_eq!(backfill_table(doc2.as_table_mut(), def.as_table()), 0);
        assert_eq!(doc2.to_string(), out);
    }
}
