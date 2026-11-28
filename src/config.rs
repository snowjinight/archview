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
    /// IO 调度（解码/预取/评级发牌让路规则）
    pub io: IoConfig,
    /// 相机 RAW 照片与非标裸数据（raw dump）配置（PLAN_raw_media.md）
    pub raw: RawConfig,
    pub player: PlayerConfig,
    /// 图片预览管线（纹理直显/缓存/高清档与观感增强调参）——原在 [ui] 节，
    /// 键多且含算法调参，独立成节。旧配置 [ui] 下遗留的 image_* 键被 serde
    /// 静默忽略（值失效），缺键由 backfill 按默认值补进 [image]；
    /// 旧布局文件需手动搬移一次（个人工具不做常驻迁移代码）
    pub image: ImageConfig,
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
    /// encode（`archview encode` CLI / UI「编码为视频」）编码参数
    pub encode: EncodeConfig,
    /// 主程序日志级别：error/warn/info/debug/trace，也支持 EnvFilter 指令
    /// 语法（如 "info,archview::ui::panels=debug" 只开某模块 debug 诊断）。
    /// RUST_LOG 环境变量优先生效
    pub log_level: String,
}

/// VarTidy 配置：仓库与报告路径集中此处，代码零硬编码（doc/架构设计/12_VarTidy整理器.md §12.6）
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

/// encode 壳编码参数（doc/架构设计/15_encode引擎.md §15.5）：壳把 vcodec 组装进 EncodeOptions，
/// crf/preset/ovcopts 组装成 ovcopts（用户项追加在后，同键后者生效 =
/// 用户显式项以用户为准；引擎再统一追加 bf=0 兜底，用户显式 bf/bframes 除外）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeConfig {
    /// ffmpeg 通用视频编码器名（mpv ovc），默认 H.264；libx265 / libsvtav1 等
    /// 需本机 libmpv 构建内置才可用
    pub vcodec: String,
    /// 恒定质量（x264/x265 crf）：越低画质越高体积越大
    pub crf: u32,
    /// 编码速度预设（ultrafast…veryslow）
    pub preset: String,
    /// 附加编码器选项（ffmpeg 通用键值对，如 "bf=3" / "threads=8"），
    /// 空串 = 无；同键覆盖 crf/preset 组装出的默认项
    pub ovcopts: String,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            vcodec: "libx264".into(),
            crf: 23,
            preset: "medium".into(),
            ovcopts: String::new(),
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
    pub prefetch_downstream_mb: u64,
    /// 上游顺带落盘最大上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置
    /// 条目（下标更小的方向 = 列表向上；7z 限于目标所在 folder 内、RAR
    /// 为整个前缀）顺带写盘，随机跳看后回看直接命中缓存。
    /// 解码零增量，仅占磁盘写入量。
    /// 语义与下游不对称：下游 prefetch_downstream_mb 是目标窗口量（尽量取满），
    /// 本键只是上限（max）——实取多少取决于免费区（folder 前缀）大小与已缓存
    /// 分布，通常小于该值，绝不回头补解凑满
    pub prefetch_upstream_max_mb: u64,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            prefetch_downstream_mb: 512,
            prefetch_upstream_max_mb: 512,
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
    /// 单媒体文件入口（拖入文件/SendTo/命令行/「打开媒体文件」）对其所在
    /// 目录的递归收集深度：1 = 只收集同级媒体文件（默认，防盘根/超大
    /// 目录全盘扫描）；N = 向下 N 层；0 = 不限制（与「打开媒体目录」一致）
    pub file_open_depth: usize,
}

/// IO 调度配置（统一 IO 代理读调度层，设计 §19）：读线程池直读源字节，
/// 读毕投回 buffer，服务 spawn 纯 CPU 解码 worker
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IoConfig {
    /// 读线程数（语义回归本源，doc/架构设计/19_IO代理读重构.md D1）：1 =
    /// 单读线程，同一时刻全系统至多一个源读（机械盘/NAS HDD 连续读，默认）；
    /// 4 = 四读线程并行（SSD 用户调大）。解码段纯 CPU 不占读线程，其并行度
    /// 不受本值约束（自然上界 = UI 拓扑 ≈ 11）。钳制 1..=16，重载即时生效
    /// （缺员补 spawn、超编自行退出，不打断在途读）
    pub io_parallel_max_num: usize,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self {
            io_parallel_max_num: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub hwdec: String,
    pub cache_secs: u64,
    pub demuxer_max_mb: u64,
    /// demux 探测字节预算（mpv demuxer-lavf-probesize，如 "2MiB"）。
    /// "auto" = 不设置，走 mpv/FFmpeg 原生默认（≈5MB）。压缩包内流每读
    /// 一字节都要解压，调小直接缩短打开耗时；过小可能识别不了冷门容器
    pub demuxer_probesize: String,
    /// demux 探测时长预算秒（mpv demuxer-lavf-analyzeduration）。
    /// "auto" = 不设置，走 mpv/FFmpeg 原生默认（≈5s）。过小可能漏识别
    /// 延迟出现的流（TS/mkv 多音轨场景）
    pub demuxer_analyzeduration: String,
    pub volume: i64,
    /// 静音状态（唯一真相，持久化）：未播放/无播放器时也可预设，
    /// 播放器创建时与 volume 一起应用
    pub muted: bool,
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
            // 末 6 个为相机 RAW（raw-files 库提取内嵌 JPEG 预览直读，走图片
            // 纹理路径）：Sony ARW 家族（arw/srf/sr2/arq）+ Nikon NEF 家族（nef/nrw）
            image_extensions: [
                "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "arw", "nef", "nrw",
                "srf", "sr2", "arq",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            file_open_depth: 1,
        }
    }
}

/// 非标裸数据（raw dump）配置：这些文件不是标准容器，无法探测参数，
/// 统一走 mpv rawvideo demuxer（视频桶），由 [[raw.demux]] 预设提供
/// 像素格式 + 宽高 + fps（经 loadfile per-file 选项下发）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawConfig {
    /// demux 预设（按声明顺序取首个 glob 命中：先写具体、后写宽泛）。
    /// 预设是裸数据唯一入口：glob 命中才可见、可播——不再有独立扩展名
    /// 清单（.yuv/.bin 等非标扩展名都只靠这里声明，防「可见但不可播」
    /// 的中间态）。默认内置五类 FPGA dump 常见后缀（nv12/yuv400/yuv420/
    /// rgb/raw），宽高交由文件名内嵌 WxH 兜底（[[raw.demux]] 数组不逐项
    /// backfill：用户已有任一预设则整用用户的，需要默认五条时手动复制）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub demux: Vec<RawDemux>,
}

impl Default for RawConfig {
    fn default() -> Self {
        RawConfig {
            demux: vec![
                // NV12 8bit 半平面 dump
                RawDemux {
                    r#match: "*.nv12".into(),
                    pixfmt: "nv12".into(),
                    fps: default_raw_fps(),
                    ..Default::default()
                },
                // YUV400 = 8bit 单色平面
                RawDemux {
                    r#match: "*.yuv400".into(),
                    pixfmt: "gray".into(),
                    fps: default_raw_fps(),
                    ..Default::default()
                },
                // YUV420 planar（I420）dump：Y/U/V 三个独立平面
                RawDemux {
                    r#match: "*.yuv420".into(),
                    pixfmt: "yuv420p".into(),
                    fps: default_raw_fps(),
                    ..Default::default()
                },
                // RGB888 逐像素 3 字节（R,G,B 字节序 = FFmpeg rgb24）
                RawDemux {
                    r#match: "*.rgb".into(),
                    pixfmt: "rgb24".into(),
                    fps: default_raw_fps(),
                    ..Default::default()
                },
                // unpacked Bayer dump：12bit 低对齐存 16bit 容器；
                // bayer_auto16le = 伪格式，装载时自动探测排列（见 raw_pixfmt_bits）
                RawDemux {
                    r#match: "*.raw".into(),
                    pixfmt: "bayer_auto16le".into(),
                    fps: default_raw_fps(),
                    bit_shift: 4,
                    ..Default::default()
                },
            ],
        }
    }
}

/// 单条 raw dump 播放预设：文件名 glob → 像素格式 + 宽高 + fps
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawDemux {
    /// 文件名通配：* 与 ?（大小写不敏感；写文件名，不含路径）
    pub r#match: String,
    /// FFmpeg 像素格式名（支持名单见 RAWVIDEO_PIXFMTS / README「raw 格式表」；
    /// 另支持伪格式 bayer_auto16le = 自动探测排列）
    pub pixfmt: String,
    /// 帧宽（像素）。省略或 0 = 从文件名内嵌 WxH 兜底（见 parse_filename_dims）
    #[serde(default)]
    pub width: u32,
    /// 帧高（像素）。省略或 0 = 从文件名内嵌 WxH 兜底
    #[serde(default)]
    pub height: u32,
    /// 帧率
    #[serde(default = "default_raw_fps")]
    pub fps: f64,
    /// 每 u16 采样左移位数（0~8，默认 0 = 不位移，高位溢出丢弃）。
    /// 适用场景：12bit 等有效位深数据 LSB 对齐（低对齐补零）存进 16bit
    /// 容器时，FFmpeg bayer16* 转 RGB 写死取高 8 位（>>8），低对齐数据
    /// 只剩 16 级灰阶——播放发黑根因。12bit→16bit 容器填 4，10bit 填 6。
    /// 约束：>0 时 pixfmt 须以 "le" 结尾（按 LE u16 采样移位）；快路径
    /// （zip 流式 / 目录模式）与慢路径（7z/RAR 落盘经单文件 VFS 借道
    /// zvf://，位移只在流层发生、缓存文件保持忠实拷贝）均生效
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub bit_shift: u32,
}

fn default_raw_fps() -> f64 {
    3.0
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// 极简通配匹配（仅 `*` 与 `?`，大小写不敏感，不引 glob crate）。
/// 经典迭代回溯：遇 `*` 记录（模式位/名字位），失配时让该 `*` 多吞一字符重试。
/// 直接在 UTF-8 字节上跑、零堆分配（万级文件逐名匹配的热路径）：`?` 与
/// `*` 回溯均按 UTF-8 字符粒度推进（char_len 跳过续字节），非 ASCII 模式/
/// 文件名逐字节相等 == 逐字符相等，语义与旧 Vec<char> 实现完全一致
fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    // ni 处 UTF-8 字符的字节长（&str 保证合法 UTF-8，ni 恒在字符边界）
    let char_len = |bytes: &[u8], i: usize| match bytes[i] {
        b if b < 0x80 => 1,
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        b if b >= 0xC0 => 2,
        _ => 1, // 孤立续字节（不可达）：按 1 字节兜底，不 panic
    };
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi].eq_ignore_ascii_case(&n[ni])) {
            pi += 1;
            ni += if p[pi - 1] == b'?' {
                char_len(n, ni)
            } else {
                1
            };
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = ni;
            pi += 1;
        } else if star != usize::MAX {
            mark += char_len(n, mark); // `*` 多吞一个字符重试
            pi = star + 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// 纯后缀形模式判定：`*` 仅在开头出现一次、其余为字面字符（`*.raw`、
/// `*_dump.raw`，`*` 单独出现 = 匹配一切也归此类）→ 命中条件退化为
/// 「大小写不敏感 ends_with」，万级文件热路径免回溯；返回 None（含中置
/// `*`/`?`）才走 glob_match 通用回溯。与 glob 语义逐点等价：星吞任意
/// （含零个）字符后接字面后缀 == ends_with 该后缀
fn suffix_pattern(pattern: &str) -> Option<&str> {
    let rest = pattern.strip_prefix('*')?;
    rest.as_bytes()
        .iter()
        .all(|&b| b != b'*' && b != b'?')
        .then_some(rest)
}

/// rawvideo 可裸读像素格式（名字 → 每像素**位数**）。
/// 名单来源：`ffmpeg -pix_fmts`（ffmpeg 7.x），筛除硬件(H)/调色板(P)/
/// bitstream(B)/无输入端口项；mpv demuxer-rawvideo-mp-format 接受同一名单。
/// 位数为 rawvideo demuxer 实际读写的**容器位数**（9~16bit 采样占 2 字节
/// 容器、rgb555/444 填零到 16bit 等），比 -pix_fmts 的逻辑位数（按分量
/// 深度累加）在 10/12bit 格式上更大——帧字节数校验以此为基才与文件
/// 实际大小吻合。MIPI packed bayer（mipi_rggb10 等）不在本名单：跨像素
/// 位打包与「容器位数」口径不符（W×H×bits÷8 整除校验不适用）——libmpv
/// 较新 ffmpeg 实际认识该名（e2e 实测，doc/架构落地开发/08_RAW媒体支持专项.md §8.3 坑 B），本表是有意
/// 收窄：前置拦截报错优先于让 mpv 试载。
pub const RAWVIDEO_PIXFMTS: &[(&str, u32)] = &[
    // Bayer（12，全部仅输入；packed MIPI 10/12bit 不在 ffmpeg 名单）
    ("bayer_bggr8", 8),
    ("bayer_rggb8", 8),
    ("bayer_gbrg8", 8),
    ("bayer_grbg8", 8),
    ("bayer_bggr16le", 16),
    ("bayer_bggr16be", 16),
    ("bayer_rggb16le", 16),
    ("bayer_rggb16be", 16),
    ("bayer_gbrg16le", 16),
    ("bayer_gbrg16be", 16),
    ("bayer_grbg16le", 16),
    ("bayer_grbg16be", 16),
    // Gray / Gray+Alpha（16）
    ("gray", 8),
    ("gray9le", 16),
    ("gray9be", 16),
    ("gray10le", 16),
    ("gray10be", 16),
    ("gray12le", 16),
    ("gray12be", 16),
    ("gray14le", 16),
    ("gray14be", 16),
    ("gray16le", 16),
    ("gray16be", 16),
    ("grayf32le", 32),
    ("grayf32be", 32),
    ("ya8", 16),
    ("ya16le", 32),
    ("ya16be", 32),
    // YUV planar 8-bit（14）
    ("yuv420p", 12),
    ("yuv422p", 16),
    ("yuv444p", 24),
    ("yuv410p", 9),
    ("yuv411p", 12),
    ("yuv440p", 16),
    ("yuvj420p", 12),
    ("yuvj422p", 16),
    ("yuvj444p", 24),
    ("yuvj440p", 16),
    ("yuvj411p", 12),
    ("yuva420p", 20),
    ("yuva422p", 24),
    ("yuva444p", 32),
    // YUV planar 9~16-bit（56；每采样 2 字节容器）
    ("yuv420p9le", 24),
    ("yuv420p9be", 24),
    ("yuv420p10le", 24),
    ("yuv420p10be", 24),
    ("yuv420p12le", 24),
    ("yuv420p12be", 24),
    ("yuv420p14le", 24),
    ("yuv420p14be", 24),
    ("yuv420p16le", 24),
    ("yuv420p16be", 24),
    ("yuv422p9le", 32),
    ("yuv422p9be", 32),
    ("yuv422p10le", 32),
    ("yuv422p10be", 32),
    ("yuv422p12le", 32),
    ("yuv422p12be", 32),
    ("yuv422p14le", 32),
    ("yuv422p14be", 32),
    ("yuv422p16le", 32),
    ("yuv422p16be", 32),
    ("yuv444p9le", 48),
    ("yuv444p9be", 48),
    ("yuv444p10le", 48),
    ("yuv444p10be", 48),
    ("yuv444p12le", 48),
    ("yuv444p12be", 48),
    ("yuv444p14le", 48),
    ("yuv444p14be", 48),
    ("yuv444p16le", 48),
    ("yuv444p16be", 48),
    ("yuv440p10le", 24),
    ("yuv440p10be", 24),
    ("yuv440p12le", 24),
    ("yuv440p12be", 24),
    ("yuva420p9le", 40),
    ("yuva420p9be", 40),
    ("yuva420p10le", 40),
    ("yuva420p10be", 40),
    ("yuva420p16le", 40),
    ("yuva420p16be", 40),
    ("yuva422p9le", 48),
    ("yuva422p9be", 48),
    ("yuva422p10le", 48),
    ("yuva422p10be", 48),
    ("yuva422p12le", 48),
    ("yuva422p12be", 48),
    ("yuva422p16le", 48),
    ("yuva422p16be", 48),
    ("yuva444p9le", 64),
    ("yuva444p9be", 64),
    ("yuva444p10le", 64),
    ("yuva444p10be", 64),
    ("yuva444p12le", 64),
    ("yuva444p12be", 64),
    ("yuva444p16le", 64),
    ("yuva444p16be", 64),
    // YUV semi-planar / NV（23）
    ("nv12", 12),
    ("nv21", 12),
    ("nv16", 16),
    ("nv24", 24),
    ("nv42", 24),
    ("p010le", 24),
    ("p010be", 24),
    ("p012le", 24),
    ("p012be", 24),
    ("p016le", 24),
    ("p016be", 24),
    ("p210le", 32),
    ("p210be", 32),
    ("p212le", 32),
    ("p212be", 32),
    ("p216le", 32),
    ("p216be", 32),
    ("p410le", 48),
    ("p410be", 48),
    ("p412le", 48),
    ("p412be", 48),
    ("p416le", 48),
    ("p416be", 48),
    // YUV packed（10）
    ("yuyv422", 16),
    ("uyvy422", 16),
    ("yvyu422", 16),
    ("ayuv64le", 64),
    ("vuya", 32),
    ("vuyx", 24),
    ("y210le", 32),
    ("y212le", 32),
    ("xv30le", 32),
    ("xv36le", 64),
    // RGB packed 8-bit（26）
    ("rgb24", 24),
    ("bgr24", 24),
    ("argb", 32),
    ("rgba", 32),
    ("abgr", 32),
    ("bgra", 32),
    ("0rgb", 24),
    ("rgb0", 24),
    ("0bgr", 24),
    ("bgr0", 24),
    ("rgb8", 8),
    ("bgr8", 8),
    ("rgb4_byte", 8),
    ("bgr4_byte", 8),
    ("rgb565le", 16),
    ("rgb565be", 16),
    ("bgr565le", 16),
    ("bgr565be", 16),
    ("rgb555le", 16),
    ("rgb555be", 16),
    ("bgr555le", 16),
    ("bgr555be", 16),
    ("rgb444le", 16),
    ("rgb444be", 16),
    ("bgr444le", 16),
    ("bgr444be", 16),
    // RGB 高位深（12）
    ("rgb48le", 48),
    ("rgb48be", 48),
    ("bgr48le", 48),
    ("bgr48be", 48),
    ("rgba64le", 64),
    ("rgba64be", 64),
    ("bgra64le", 64),
    ("bgra64be", 64),
    ("x2rgb10le", 32),
    ("x2bgr10le", 32),
    ("rgbaf16le", 64),
    ("rgbaf16be", 64),
    // Planar RGB / GBR（24；高位深每采样 2 字节容器）
    ("gbrp", 24),
    ("gbrp9le", 48),
    ("gbrp9be", 48),
    ("gbrp10le", 48),
    ("gbrp10be", 48),
    ("gbrp12le", 48),
    ("gbrp12be", 48),
    ("gbrp14le", 48),
    ("gbrp14be", 48),
    ("gbrp16le", 48),
    ("gbrp16be", 48),
    ("gbrpf32le", 96),
    ("gbrpf32be", 96),
    ("gbrap", 32),
    ("gbrap10le", 64),
    ("gbrap10be", 64),
    ("gbrap12le", 64),
    ("gbrap12be", 64),
    ("gbrap14le", 64),
    ("gbrap14be", 64),
    ("gbrap16le", 64),
    ("gbrap16be", 64),
    ("gbrapf32le", 128),
    ("gbrapf32be", 128),
    // XYZ（2）
    ("xyz12le", 48),
    ("xyz12be", 48),
];

/// 查 pixfmt 名单：Some = 每像素位数（帧位数 = W×H×bits，帧字节数 = /8，
/// 供不读文件的整除校验）；None = 名字不合法/不可裸读（含 MIPI packed
/// bayer、调色板 pal8、硬件格式），调用方报错并列出支持名单
pub fn raw_pixfmt_bits(pixfmt: &str) -> Option<u32> {
    // 伪格式（不在 RAWVIDEO_PIXFMTS / ffmpeg 名单）：16bit 容器 Bayer，
    // 排列（RGGB/BGGR/GRBG/GBRG）装载时由像素统计自动探测（仅播放链路；
    // encode 引擎引用本函数的调用点须显式拦截，防伪格式漏进 ffmpeg 滤镜图）
    if pixfmt.eq_ignore_ascii_case("bayer_auto16le") {
        return Some(16);
    }
    RAWVIDEO_PIXFMTS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(pixfmt))
        .map(|(_, b)| *b)
}

/// 从文件名提取内嵌分辨率候选（按出现顺序）：`WxH`（分隔符 x/X）。
/// 边界规则：数字段两端不得邻接字母/数字（防 v2x4、mp3x4 类误匹配；
/// "0x10" 十六进制因 W=0 天然被拒）；段长 1~5 位数字（≤99999）。
/// 返回空 = 文件名不含可用分辨率。例：
/// `fpga_in_s0_1920x1080_00_12345.raw` → [(1920, 1080)]
pub fn parse_filename_dims(name: &str) -> Vec<(u32, u32)> {
    let b = name.as_bytes();
    // x 位置左侧数字串起点（b[s..x] 全数字且 s-1 非字母数字）
    let digits_before = |x: usize| -> Option<usize> {
        let mut s = x;
        while s > 0 && b[s - 1].is_ascii_digit() {
            s -= 1;
        }
        let len = x - s;
        if len == 0 || len > 5 || (s > 0 && b[s - 1].is_ascii_alphanumeric()) {
            return None;
        }
        Some(s)
    };
    // x 位置右侧数字串区间 [x+1, e)（e 处非字母数字）
    let digits_after = |x: usize| -> Option<(usize, usize)> {
        let mut e = x + 1;
        while e < b.len() && b[e].is_ascii_digit() {
            e += 1;
        }
        let len = e - (x + 1);
        if len == 0 || len > 5 || (e < b.len() && b[e].is_ascii_alphanumeric()) {
            return None;
        }
        Some((x + 1, e))
    };
    let mut out = Vec::new();
    for i in 0..b.len() {
        if b[i] != b'x' && b[i] != b'X' || i == 0 {
            continue;
        }
        if let (Some(s), Some((rs, re))) = (digits_before(i), digits_after(i)) {
            // ≤5 位数字串解析必成功不溢出
            let w: u32 = std::str::from_utf8(&b[s..i])
                .ok()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            let h: u32 = std::str::from_utf8(&b[rs..re])
                .ok()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            if w > 0 && h > 0 {
                out.push((w, h));
            }
        }
    }
    out
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            hwdec: "auto".into(),
            cache_secs: 30,
            demuxer_max_mb: 512,
            demuxer_probesize: "2MiB".into(),
            demuxer_analyzeduration: "1".into(),
            volume: 100,
            muted: false,
            mpv_log_level: "no".into(),
        }
    }
}

/// 白边抑制默认曲线（imgview 构建查表与本 Default/模板共用，单一来源，
/// 2026-11-18 用户 release 实测定版）：100 前几乎不抑制（细纹理不误伤），
/// 100→150 轻压，150→200/200→240 渐进，240→255 收到 0.05（极强边保留
/// 5% 锐感防钝化）
pub const DEFAULT_WHITE_EDGE_CURVE: [(f32, f32); 5] = [
    (100.0, 1.0),
    (150.0, 0.9),
    (200.0, 0.6),
    (240.0, 0.3),
    (255.0, 0.05),
];

/// 图片预览管线配置（[image] 节）：纹理直显开关/缓存/尺寸档位 + 高清档
/// 观感增强（降噪+锐化）算法调参。键名沿用旧 image_* 前缀（迁移纯搬移不重命名）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    /// 图片 egui 纹理直显（后台 image crate 解码 → 纹理，跳过 mpv；静图
    /// 与动图 gif 均走此链路，解码失败自动回退 mpv）
    pub image_texture: bool,
    /// 图片纹理 LRU 缓存张数（含当前张）：±1 相邻页预解码，翻页零等待
    pub image_cache_count: usize,
    /// 基础纹理档尺寸上限：预放大（×ppp）后超过则原尺寸直进；full 高清
    /// 升级资格判据同此值（见 imgview.rs resample 两档策略）
    pub image_max_dim: u32,
    /// orig-lru（CPU 原图缓存）总预算 MB：原图 DynamicImage 驻留上限，
    /// 超出逐出最旧（当前显示条目受保护）。原图缓存服务两个消费者——
    /// ① 像素探针（alt+点击读原图原值，任何尺寸都有读数）；
    /// ② full 高清升级免二次熵解码 + per-file 亮度 [ ] 快捷键免重解码
    /// （native 重建 base 纹理，毫秒级）。单张准入 = 预算/2（防单张巨图
    /// 独占预算），超准入的图不驻留（探针/full 复用/亮度重放退回旧行为）。
    /// 默认 512（≈容纳 85MP RGB8 ×2 或 8K 图 ×3）；钳 64~4096
    pub image_orig_cache_mb: u32,
    /// 高清档（full）触发/换画阈值：屏幕物理像素超过基础纹理该倍数
    /// （zoom×ppp > 此值）才解码/换画 full 级纹理。默认 2.0 = badge 200%
    /// 起接管——full 是 ×3 密度纹理，badge 300%（采样比 zoom×ppp/3 = 1）
    /// 处逐纹素直出、锐度完整；200%~300% 为 0.67~1× 轻微缩采样（静止图
    /// 无感）；调大（如 3.0）甜点后移且基本关闭
    pub image_full_threshold: f32,
    /// 高清档放大前在原图分辨率执行观感增强（双边降噪 → unsharp 锐化，
    /// 把边缘坡度先变陡，Lanczos 插值保持陡峭过渡；实测深放大观感增益
    /// 明显，默认开——噪点同步放大由前置降噪对冲，强对比边缘可能有轻微
    /// 光晕，关掉可 A/B 对比）
    pub image_full_sharpen: bool,
    /// unsharp 锐化强度 amount（0~2，默认 0.5）：out = px + amount×
    /// (px − blur(px))——差值（原图 − 模糊参照）的放大倍数，正向增强另乘
    /// 白边抑制比（见 image_full_sharpen_white_edge_curve）。越大"细节提出再加回"的
    /// 力度越猛：边缘越脆，超 ~0.8 光晕/振铃明显；噪点随 amount 同步放大
    /// ——控噪优先调 sigma/降噪，而不是减 amount
    pub image_full_sharpen_amount: f32,
    /// unsharp 半径 σ 像素（0.1~4，默认 1.5）：blur_σ 在 σ 尺度上做加权
    /// 平均，σ 越大平均范围越大——细高频（噪点、细纹理）模糊后剩得越少，
    /// 差值项里占比越低、被强化的量越少（天然抗噪点放大）；而中大尺度
    /// 边缘（轮廓等）在差值里仍存在，成为主要强化对象。即 σ 决定强化
    /// 频带，越大频带越低："颗粒感"变"体积感"，光晕宽度也 ~σ。σ1.5 +
    /// 双边 3×3 组合实测暗部投影分层（双边小窗口平滑弱，量化台阶残留被
    /// 大 σ 锐化拉开），导向 r≥2 平滑强无此问题——分层时优先切导向或降
    /// σ，而不是只降 σ。JPEG 块效应等低频压缩噪例外——升 σ 反而会被当
    /// 结构拾取放大，此时降 amount 或关锐化
    pub image_full_sharpen_sigma: f32,
    /// 白边抑制曲线（config image_full_sharpen_white_edge_curve）：折点
    /// [shoot_v, ratio] 数组，折点间分段线性插值（隐含首点 [0,1]，末点后
    /// 平坦 = 末点 ratio）。shoot_v = 逐点 3×3/5×5/7×7 多尺度亮度方差按
    /// 相机 ISP shoot.c mode 0 整数组合出的强边标记（0~255，大 = 孤立强边
    /// ——unsharp 白边亮晕的来源；细纹理/噪点 shoot_v≈0）。正向增强乘
    /// ratio（只压亮侧 d>0），暗侧压暗不受影响。曲线语义：shoot_v 小时
    /// ratio≈1 细纹理全额保留，到拐点后渐进压到 0——比单斜率直线观感好
    /// （直线低段把弱边也压太多，用户实测弃用）。校验：x ∈ (0,255] 严格
    /// 递增、y ∈ [0,1]，非法回退默认曲线并 WARN；空数组 = 关闭（连方差
    /// 计算都跳过）。末点 [255, 0.05] = 极强边保留 5% 增强不掉锐感
    pub image_full_sharpen_white_edge_curve: Vec<(f32, f32)>,
    /// shoot 敏感度增益（config image_full_sharpen_white_edge_gain，默认
    /// 1.0）：乘在跨尺度方差差（v7−v3、v7−v5，**方向保留不取 abs**——实测
    /// abs 响应毛躁、点亮细结构反转差值导致结构紊乱；方向保留只响应宽边
    /// flank，边缘带干净但偏在真边缘外侧 1~2px）上再组合 shoot_v。缓坡等
    /// 边缘的方差差量级小、落在 0..255 低段——增益把它线性展宽到全程，
    /// 让默认曲线拐点（150 起）对得上。1.0 = 最接近原始 shoot.c 语义；
    /// 使用 clamp [1, 1024]
    pub image_full_sharpen_white_edge_gain: f32,
    /// shoot flank 带外扩半径（config image_full_sharpen_white_edge_dilate，
    /// 0~4 默认 1，0 = 关）：方向保留的方差差响应呈"高亮带在真边缘外侧
    /// 1~2px"（边缘正中 v3≈v7≈v5 差值收敛为低值）——max 膨胀外扩
    /// （1 = 外扩 1px，2 = 外扩 2px）覆盖真边缘线本身。ratio、dump、防振铃
    /// 掩码都吃外扩后的 shoot_v（所见即所得）；clamp [0, 4]
    pub image_full_sharpen_white_edge_dilate: u32,
    /// 锐化前保边降噪（压平坦区噪点不糊边缘；默认开）
    pub image_full_denoise: bool,
    /// 降噪算法切换："guided"（导向滤波，默认——A/B 实测锯齿最弱、弱纹理
    /// 保留最好）/"bilateral"（双边 3×3，眼部更锐但弱纹理敏感）。
    /// 两算法参数独立、用对应算法名前缀区分（image_full_bilateral_* /
    /// image_full_guided_*），strength 同量纲（亮度差容忍级）可同值对比。
    /// 非法值回退导向并 WARN（不静默）
    pub image_full_denoise_algo: String,
    /// 双边降噪颜色容忍度 σc（0~100 亮度级，默认 10，仅 algo = "bilateral"
    /// 生效）：亮度差小于它的邻点参与平均。影响档位：
    ///   5~10 = 温和：只压最细噪点，毛孔/弱纹理基本全保，噪点有残留；
    ///   10~20 = 通用：平坦区噪点压净，皮肤渐变开始轻微涂抹；
    ///   20~50 = 激进：噪点（含中尺度斑块）压得净，弱纹理明显抹平；
    ///   >50 ≈ 普通模糊：绝大多数邻点参与平均，边缘也糊。
    /// 涂抹感重 → 调小；噪点残留 → 调大
    pub image_full_bilateral_strength: f32,
    /// 导向降噪结构容忍度（0~100 亮度级，默认 5，仅 algo = "guided" 生效）：
    /// 语义 = 局部亮度差容忍 D，内部换算 ε = D²（方差阈值）——方差 ≪ ε 的
    /// 区域（平坦/噪点）被平均压噪，方差 ≫ ε 的区域（边缘/渐变）原值保留。
    /// 影响档位（同等数值下涂抹感轻于双边——判据是整窗方差而非点到点差）：
    ///   3~8 = 温和（默认 5）：只压细噪点，皮肤渐变/头发过渡带完整保留；
    ///   15~30 = 通用（高 ISO 重噪）：噪点幅度大，ε 提到 std 10~15 级才压得住；
    ///   30~60 = 激进（胶片颗粒/老照片）：中尺度斑块也压，弱纹理明显涂抹；
    /// 注意噪点本身抬高局部方差——重噪图需要更大 D 才压得住。
    /// 涂抹感重 → 调小；噪点残留 → 调大
    pub image_full_guided_strength: f32,
    /// 导向滤波窗口半径（1~8，默认 1，仅 algo = "guided" 生效）。影响档位：
    ///   1（3×3）= 默认（人像实测定版）：刚好覆盖 1~2px 噪点自相关尺度，
    ///     方差估计最局部、弱纹理判据最敏锐；大窗口会把发丝间隙/毛孔的
    ///     方差被窗口平均稀释 → 误判平坦 → 涂抹/锯齿；
    ///   2~4（5×5~9×9）= 重噪图（噪点成团/中尺度斑块）；
    ///   5~8 = 低频色斑也能压，但平坦区涂抹感明显、弱纹理发糊。
    /// 时间与半径无关（O(1)/px 滑动均值），内存亦无关
    pub image_full_guided_radius: u32,
    /// 防振铃（默认开）：Lanczos3 负瓣在强边两侧产生越出源邻域值域的
    /// 过冲/欠冲（Gibbs 振铃，深放大后白线/暗线）——放大输出像素 clamp
    /// 回源对应 cell 逐通道 min/max：域内合法信号（边缘过渡/细纹理/1px
    /// 亮线）恒等映射不动、越界值必为插值伪像才被剪，锐度无损（mpv
    /// zscale antiring 同思路）。与 shoot 白边抑制互补：shoot 治放大前
    /// unsharp 白边，这个治放大时 Lanczos 振铃。仅高清放大档生效
    pub image_full_antiring: bool,
    /// 对比视图 full 重采样使能（默认关）：多图对比下主栏深放大是否
    /// 解码 ×ppp×3 高密度纹理。对比的目的是"同屏看差异"，base 档纹理
    /// （×ppp 预放大）已够用，full 档主栏与 base 档副栏锐度不一致反而
    /// 干扰判断，且逐次主栏切换触发的 full 解码是纯性能开销——故默认关，
    /// 单图模式不受此键影响（恒允许升级）
    pub image_compare_full_resample_enable: bool,
    /// debug：锐化算法 dump shoot 边缘图（默认关）。开启后每次高清档解码把
    /// shoot_v（多尺度方差强边标记 0~255）直出灰度 bmp 到缓存目录
    /// debug/sharpen_shoot_dump.bmp（固定名覆盖）：白 = 强边（白边抑制
    /// 生效区）、黑 = 细纹理。用于肉眼验证边缘检测是否正确
    pub image_full_sharpen_dump_shoot: bool,
    /// debug：锐化算法 dump 白边抑制曲线查表（默认关）。开启后把实际使用
    /// 的 shoot_v→ratio 插值结果写缓存目录 debug/sharpen_curve_dump.txt
    /// （256 行，固定名覆盖）：用于验证配置折点的分段线性插值是否正确生效
    pub image_full_sharpen_dump_curve: bool,
    /// debug：锐化算法 dump 前后 Y 通道（默认关）。锐化为 Y 通道单通道
    /// unsharp（色度不锐化），开启后把锐化输入 Y（降噪后）与锐化输出 Y
    /// 各存一张 8bit 灰度 bmp（debug/sharpen_y_before.bmp、
    /// debug/sharpen_y_after.bmp，固定名覆盖）：用于逐点核对锐化在亮度上
    /// 做了什么
    pub image_full_sharpen_dump_y: bool,
    /// debug：锐化算法 dump shoot 白边抑制前后的增强量（默认关）。开启后
    /// 把"未抑制增强量"（amount×d）与"抑制后增强量"（amount×d×ratio）各
    /// 存一张 8bit 灰度 bmp（debug/sharpen_delta_before.bmp、
    /// debug/sharpen_delta_after.bmp，固定名覆盖）：差异值有正有负，**整体
    /// +128 偏移**（128 = 零点增强），两图相减即白边抑制实际削掉的量，
    /// 用于核对抑制效果是否对准 halo
    pub image_full_sharpen_dump_enhance: bool,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            image_texture: true,
            image_cache_count: 3,
            image_max_dim: 4096,
            image_orig_cache_mb: 512,
            image_full_threshold: 2.0,
            image_full_sharpen: true,
            image_full_sharpen_amount: 1.0,
            image_full_sharpen_sigma: 1.5,
            image_full_sharpen_white_edge_curve: DEFAULT_WHITE_EDGE_CURVE.to_vec(),
            image_full_sharpen_white_edge_gain: 1.0,
            image_full_sharpen_white_edge_dilate: 1,
            image_full_denoise: true,
            image_full_denoise_algo: "guided".into(),
            image_full_bilateral_strength: 10.0,
            image_full_guided_strength: 10.0,
            image_full_guided_radius: 1,
            image_full_antiring: true,
            image_compare_full_resample_enable: false,
            image_full_sharpen_dump_shoot: false,
            image_full_sharpen_dump_curve: false,
            image_full_sharpen_dump_y: false,
            image_full_sharpen_dump_enhance: false,
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
    /// 预览窗口内滚轮切换媒体的最小间隔秒数（防惯性/触控板连跳）
    pub wheel_switch_secs: f64,
    /// 翻到本包最后一个媒体项再往后导航时，是否询问打开同目录下一个压缩包
    pub ask_next_archive: bool,
    /// 文件列表是否按自然序排序（数字段按数值：1/2/10；不区分大小写）。
    /// false（默认）= 保持压缩包内部顺序：7z/RAR 固态包下内部顺序 == 物理码流顺序，
    /// 预取直接命中物理相邻文件（性能最优）。注意 7-Zip 工具打的包内部本来就是
    /// 字典序，关闭排序对这类包显示无变化。
    /// true = 自然序：看漫画/图集时开启，编号文件按人类预期排列（1/2/10）。
    /// 目录模式不看此键，强制自然序（本地文件直读排序零代价）
    pub sort_files: bool,
    /// 文件列表搜索防抖（毫秒）：停止输入该时长后才执行一次过滤重建
    /// （万级条目的大包逐键重建才有感卡顿）。0 = 不防抖，每键即时过滤
    pub search_debounce_ms: u64,
    /// 解压覆盖层显示前的宽限期（毫秒）：正在播放视频时切换到需解压的条目，
    /// 超过该时长仍未解完才隐藏视频窗口并显示进度覆盖层——快速解压
    /// （缓存命中/小文件）全程不打扰播放，避免窗口闪烁
    pub extract_overlay_delay_ms: u64,
    /// UI 重绘频率三档（fps）——悬停移动（指针滑过 UI、未按下）上限。
    /// egui-winit 对鼠标移动无条件 request_repaint，事件率（125~1000Hz）
    /// 直接驱动重绘频率，而立即模式 UI 每帧全量重建+present，低端机上悬停
    /// 占用可观。30fps 高亮跟随无感知、占用减半；弱机可再调低。钳制 5~240
    pub redraw_hover_fps: u32,
    /// UI 重绘频率三档——拖拽中（指针按下：平移/缩放/拖动进度条/拖列表）
    /// 上限：需要更高跟手度，默认 60
    pub redraw_drag_fps: u32,
    /// UI 重绘频率三档——其余情况（线程唤醒/timer/hover 动画/后台事件
    /// 刷新）上限：解码完成显示等事件延迟 ≤ 帧间隔，默认 60 平衡占用与响应
    pub redraw_other_fps: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            font_path: "auto".into(),
            slideshow_secs: 1.0,
            wheel_switch_secs: 0.3,
            ask_next_archive: true,
            sort_files: false,
            search_debounce_ms: 200,
            extract_overlay_delay_ms: 500,
            redraw_hover_fps: 30,
            redraw_drag_fps: 60,
            redraw_other_fps: 60,
        }
    }
}

impl UiConfig {
    /// 帧率上限 → 节流间隔（毫秒）。fps 钳制 5~240：下限防 0 除/超长睡眠，
    /// 上限防配置误填导致节流失效退回事件风暴
    pub fn pacing_ms(fps: u32) -> u64 {
        (1000.0 / fps.clamp(5, 240) as f64).round() as u64
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache: CacheConfig::default(),
            media: MediaConfig::default(),
            io: IoConfig::default(),
            raw: RawConfig::default(),
            player: PlayerConfig::default(),
            image: ImageConfig::default(),
            ui: UiConfig::default(),
            extract: ExtractConfig::default(),
            codepage: "gbk".into(),
            passwords: Vec::new(),
            vartidy: VarTidyConfig::default(),
            encode: EncodeConfig::default(),
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

    /// 当前生效的 config.toml 路径（「配置」菜单：外部编辑器打开 / 热重载）。
    /// 返回候选中第一个存在的；都不存在 = exe 同目录（启动时生成默认模板处）
    pub fn config_path() -> PathBuf {
        let cands = Self::config_candidates();
        let fallback = cands[0].clone();
        cands.into_iter().find(|p| p.exists()).unwrap_or(fallback)
    }

    /// 启动时读取 config.toml；不存在则写入带中文注释的默认模板。
    /// 热重载（菜单「配置 → 重载配置」）复用本函数：文件被运行中删除时
    /// 会重建默认模板，缺键补全（backfill）同样生效
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
        let Ok(mut doc) = user_text.parse::<DocumentMut>() else {
            return;
        };
        let Ok(def) = DEFAULT_TOML.parse::<DocumentMut>() else {
            return;
        };
        if backfill_table(doc.as_table_mut(), def.as_table()) > 0 {
            let _ = std::fs::write(path, doc.to_string());
        }
    }

    pub fn is_video_ext(&self, name: &str) -> bool {
        // 标准 ∨ raw 预设 glob 命中（.yuv/.bin 等非标扩展名仅靠
        // [[raw.demux]] 进入视频桶，走 mpv rawvideo demuxer）——
        // 一处改动，列表/标签页/同类跳/单文件打开全部跟随
        Self::has_ext(&self.media.video_extensions, name) || self.raw_demux(name).is_some()
    }

    pub fn is_image_ext(&self, name: &str) -> bool {
        Self::has_ext(&self.media.image_extensions, name)
    }

    /// 多条 [[raw.demux]] 预设按声明顺序取首个 glob 命中
    /// （先写具体如 sensor_a_*.bin、后写宽泛如 *.bin）；无命中返回 None。
    /// 热路径分流：纯后缀形模式（`*.raw` 等，见 suffix_pattern）走大小写
    /// 不敏感 ends_with（免回溯零分配），其余才走 glob_match 通用回溯——
    /// 内置五条默认预设全落快路，万级文件目录扫描匹配成本降 ~2 个数量级
    pub fn raw_demux(&self, name: &str) -> Option<&RawDemux> {
        let file = name
            .rsplit(|c| c == '/' || c == '\\')
            .next()
            .unwrap_or(name);
        self.raw
            .demux
            .iter()
            .find(|d| match suffix_pattern(&d.r#match) {
                Some(suf) => {
                    let (f, s) = (file.as_bytes(), suf.as_bytes());
                    f.len() >= s.len() && f[f.len() - s.len()..].eq_ignore_ascii_case(s)
                }
                None => glob_match(&d.r#match, file),
            })
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
#   * 生效时机标记：【重启生效】= 顶栏「重载配置」对此项无效，须重启程序；
#     【下次播放】= 重载后在下一次播放时应用（播放中不断流）；
#     未标记 = 重载配置即时生效
# =============================================================

# 非 UTF-8 字节兜底代码页：老工具（老 WinRAR/好压等）按系统 ANSI 代码页打包，
# 密码与 zip 文件名同源编码。用于密码候选追加 + zip 文件名解码兜底
# （无 UTF-8 标志且非合法 UTF-8 的名字，中文包即 GBK）。
# 取值如 "gbk" / "big5" / "shift_jis" / "euc-kr"；"none" = 只用 UTF-8
# 重载即时写入，但作用于下次打开的压缩包（已打开包按打开时的码页）
codepage = "gbk"

# 加密包密码候选列表（播放器与 VarTidy 公用，按序尝试）。
# 打开压缩包阶段即自动探测：无密码优先 → 本列表依次；全不匹配时播放器才弹
# 密码框、VarTidy 记失败跳包。密码始终用双引号包裹；仅用于解压，不写入日志/报告
# 重载后对下次打开的压缩包生效（已打开包的密码已锁定）
passwords = []

# 主程序日志级别：error / warn / info / debug / trace。
# 也支持按模块指令（EnvFilter 语法）只开某模块的诊断日志，如帧节奏探针：
#   log_level = "info,archview::ui::panels=debug"
# RUST_LOG 环境变量优先生效
# 【重启生效】：日志订阅器启动时创建后固定，重载配置不改变运行中级别
log_level = "info"

# ============ 解压缓存（目录 / 容量 / 清理） ============
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

# ============ 媒体扩展名与目录收集 ============
[media]

# 视频 / 图片的文件扩展名识别列表（不区分大小写）
video_extensions = ["mp4", "mkv", "avi", "mov", "flv", "ts", "webm", "wmv", "m4v"]

# 相机 RAW（内嵌预览直读）：Sony ARW 家族 + Nikon NEF 家族
image_extensions = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff",
                    "arw", "nef", "nrw", "srf", "sr2", "arq"]

# 打开单个媒体文件（拖入文件/发送到/文件菜单「打开媒体文件」）时，对其
# 所在目录的递归收集深度：1 = 只收集同级媒体文件（默认，防盘根/超大
# 目录全盘扫描）；N = 向下 N 层；0 = 不限制（与「打开媒体目录」一致）
file_open_depth = 1

# ============ IO 调度（代理读线程池） ============
[io]

# 读线程数：1 = 单读线程，同一时刻全系统至多一个源读（机械盘/NAS HDD
# 连续读，默认）；4 = 四读线程并行（SSD 用户调大）。调度器按优先级队列
# 取任务（解码 > 预取 > 评级，各队列 FIFO 保序），读毕把 buffer 投回
# 服务，CPU 解码段不占读线程、并行度不受本值约束。重载配置即时生效
# （缺员补 spawn、超编自行退出，不打断在途读；0 钳回 1，上限 16）
io_parallel_max_num = 1

# ============ 非标裸数据（raw dump）预设 ============
[raw]

# 非标裸数据：没有扩展名清单——文件必须在下方 [[raw.demux]] 命中预设
# 才可见、可播放（预设即参数来源：像素格式 + 宽高；帧数由文件大小自动
# 得出，大小必须能被单帧大小整除，否则报错）。
# 可写任意多条预设，按声明顺序取首个 match 命中（先写具体、后写宽泛）。
# fps 可省略，默认 3；width/height 可省略或填 0 = 从文件名内嵌 WxH
# 兜底（如 fpga_in_s0_1920x1080_00_1.raw → 1920x1080；文件名无分辨率
# 且预设宽高为 0 时报错）。内置五条默认预设如下（可改可删；已有自定义
# 预设的老配置不会被自动合并，需要时手动复制）：

# NV12 8bit 半平面 dump
[[raw.demux]]
match = "*.nv12"
pixfmt = "nv12"

# YUV400 = 8bit 单色平面
[[raw.demux]]
match = "*.yuv400"
pixfmt = "gray"

# YUV420 planar（I420）dump：Y/U/V 三个独立平面
[[raw.demux]]
match = "*.yuv420"
pixfmt = "yuv420p"

# RGB888 逐像素 3 字节（R,G,B 字节序 = FFmpeg rgb24）
[[raw.demux]]
match = "*.rgb"
pixfmt = "rgb24"

# unpacked Bayer dump（12bit 低对齐存 16bit 容器）：
# bayer_auto16le = 伪格式，装载时读首帧自动探测排列
# （RGGB/BGGR/GRBG/GBRG，置信不足按 RGGB）；需固定排列时写具体名
[[raw.demux]]
match = "*.raw"
pixfmt = "bayer_auto16le"
bit_shift = 4

# 自定义示例（仿写后去掉行首 #；写具体宽高优先于文件名解析）：
# [[raw.demux]]
# match = "sensor_a_*.bin"    # 通配 * 与 ?；写文件名（不含路径）
# pixfmt = "bayer_rggb16le"   # FFmpeg 像素格式名（如 yuv420p / nv12 / gray）
# width = 1920
# height = 1080
# fps = 30
# bit_shift = 4               # 可选（默认 0）：12bit 低对齐存 16bit 容器时
#                             # 每 u16 采样左移位数（10bit 填 6），修正
#                             # bayer16 播放发黑；须 le 结尾格式，仅流式/
#                             # 目录模式生效（7z/RAR 落盘不支持，设计定案）

# ============ 播放器（mpv 参数） ============
[player]

# 硬件解码："auto" / "no" / "d3d11va" 等 mpv 取值
hwdec = "auto"

# mpv 缓存秒数（拖回刚看过的片段直接命中缓存，零成本）
cache_secs = 30

# mpv demuxer 最大缓存（MB）
demuxer_max_mb = 512

# demux 探测字节预算（打开阶段识别容器格式的读取上限）。
# 压缩包内流=逐字节解压，预算直接换算成打开耗时，调小即提速；
# "auto" = mpv 原生默认（≈5MB）；过小可能识别不了冷门容器
demuxer_probesize = "2MiB"

# demux 探测时长预算（秒，确定各流参数的分析上限）。
# "auto" = mpv 原生默认（≈5s）；过小可能漏识别延迟出现的音轨/字幕流
demuxer_analyzeduration = "1"

# 初始音量（0-100）
volume = 100

# 静音（重启保持；未播放时也可预先设置，播放器创建时生效）
muted = false

# mpv 内部日志级别（no/fatal/error/warn/info/status/v/debug/trace）。
# 诊断 mpv 内部行为（demux 探测/hwdec/解码链路）时临时设 "v"——
# 量极大，转发到主程序日志；平时保持 "no"
mpv_log_level = "no"

# ============ 界面 ============
[ui]

# CJK 字体路径；"auto" = 自动加载系统中/日/韩候选字体链（支持中日韩文件名显示）
# 也可指定单个字体文件的绝对路径
font_path = "auto"

# 图片自动播放（PPT 式翻页）间隔秒数
slideshow_secs = 1.0

# 预览窗口内滚轮切换媒体的最小间隔秒数（防惯性/触控板连跳）
wheel_switch_secs = 0.3

# 翻到本包最后一项再往后导航时，是否弹窗选择同目录其它压缩包
ask_next_archive = true

# 文件列表是否按自然序排序（数字段按数值：1/2/10；不区分大小写）。
# false（默认）= 保持压缩包内部顺序：固态包下内部顺序 == 物理码流顺序，
#   预取直接命中物理相邻文件（翻页性能最优）。
#   注意：7-Zip 工具打的包内部本来就是字典序，关闭排序对这类包显示无变化。
# true = 自然序：看漫画/图集时开启，编号文件按人类预期排列（1/2/10，
#   而非字典序的 1/10/2）
# 目录模式（本地文件夹）不看此键，强制自然序——本地文件直读排序零代价
sort_files = false

# 文件列表搜索防抖（毫秒）：停止输入该时长后才执行一次过滤。
# 0 = 不防抖，每键即时过滤（小包最顺手）
search_debounce_ms = 200

# 解压进度覆盖层显示前的宽限期（毫秒）：播放中切换到需解压的条目，
# 超过该时长仍未解完才隐藏视频窗口显示进度；快速解压全程不打扰播放
extract_overlay_delay_ms = 500

# ============ UI 重绘频率 ============
# egui 立即模式 UI 每帧全量重建，鼠标移动/线程唤醒会按事件率驱动重绘。
# 三档重绘频率上限把重绘封顶，静置/悬停时 CPU/GPU 占用可控（弱机显著
# 受益）。取值钳制 5~240；视频播放不受影响（vsync 主钟）

# 悬停/滚轮（指针滑过 UI、滚动列表，未按下）重绘频率上限。30 = 高亮
# 跟随与列表滚动无感知、占用约为 60fps 的一半；弱机可调 20；高刷屏
# 想更顺滑可调回 60
redraw_hover_fps = 30

# 拖拽中（指针按下：对比栏平移/缩放、拖进度条、拖列表）重绘频率上限
redraw_drag_fps = 60

# 其余情况（解码完成显示、解压进度刷新、hover 动画等）重绘频率上限。
# 调低会让后台事件显示延迟变大（间隔 = 1000/该值 毫秒）
redraw_other_fps = 60

# ============ 图片预览管线 ============
[image]

# 图片 egui 纹理直显（后台解码直接上纹理，翻页秒开；静图与动图 gif 均
# 走此链路）。解码失败的图片自动回退 mpv
image_texture = true

# 图片纹理 LRU 缓存张数（含当前张）：预解码 ±1 相邻页，翻页零等待
image_cache_count = 3

# 基础纹理档尺寸上限：预放大（×ppp）后超过则原尺寸直进；full 高清
# 升级资格判据同此值（详见 doc/架构设计/13_渲染架构迁移.md §13.4 两档策略）
image_max_dim = 4096

# orig-lru（CPU 原图缓存）总预算 MB：超出逐出最旧（当前显示受保护）。
# 服务像素探针（alt+点击任何尺寸都有原图读数）、full 升级免二次解码、
# per-file 亮度 [ ] 快捷键免重解码（native 重建，毫秒级）。单张准入 =
# 预算/2，超准入不驻留。默认 512；钳 64~4096
image_orig_cache_mb = 512

# 高清档（full）触发/换画阈值：屏幕物理像素超过基础纹理该倍数
# （zoom×ppp > 此值）才解码/换画 full 级纹理。默认 2.0 = badge 200% 起
# 接管——full ×3 密度纹理在 badge 300%（采样比 = 1）处逐纹素直出、锐度
# 完整；200%~300% 轻微缩采样静止图无感；调大（如 3.0）甜点后移且基本
# 关闭
image_full_threshold = 2.0

# 高清档放大前在原图分辨率执行观感增强（双边降噪 → unsharp 锐化，边缘
# 坡度先变陡，Lanczos 插值保持陡峭过渡；实测深放大观感增益明显，默认开；
# 噪点同步放大由前置降噪对冲，强对比边缘可能有轻微光晕，关掉可 A/B 对比）
image_full_sharpen = true

# unsharp 锐化强度 amount（0~2）：out = px + amount×(px − blur(px))——差值
# （原图 − 模糊参照）的放大倍数，正向增强另乘白边抑制比（见
# image_full_sharpen_white_edge_curve）。越大"细节提出再加回"的力度越猛：边缘越脆，
# 超 ~0.8 光晕/振铃明显；噪点随 amount 同步放大——控噪优先调 sigma/降噪，
# 而不是减 amount
#
# 场景参数参考（strength/radius 为导向算法；amount 随降噪强度联动——
# 降噪留的高频多可锐猛些，降噪猛则锐化收敛，避免锐化把涂抹痕迹放大）：
#   室内人像（默认）        strength 10   radius 1   amount 1.0
#   暗光/高ISO（夜景弱光）  strength 15~25  radius 1~2  amount 0.4~0.45
#   风景/建筑（白天低ISO）  strength 3 或降噪关  radius 1  amount 0.5
#   漫画/线稿/截图          降噪关（image_full_denoise = false）  amount 0.3~0.4
#   JPEG 重压缩网络图       strength 8~10  radius 2  amount 0.3~0.35
#   胶片扫描/老照片颗粒     strength 20~30  radius 2  amount 0.3~0.4
image_full_sharpen_amount = 1

# unsharp 半径 σ 像素（0.1~4）：blur_σ 在 σ 尺度上做加权平均，σ 越大平均
# 范围越大——细高频（噪点、细纹理）模糊后剩得越少，差值项里占比越低、
# 被强化的量越少（天然抗噪点放大）；而中大尺度边缘（轮廓等）在差值里仍
# 存在，成为主要强化对象。即 σ 决定强化频带，越大频带越低："颗粒感"变
# "体积感"，光晕宽度也 ~σ。σ1.5 + 双边 3×3 组合实测暗部投影分层（双边小
# 窗口平滑弱，量化台阶残留被大 σ 锐化拉开），导向 r≥2 平滑强无此问题
# ——分层时优先切导向或降 σ，而不是只降 σ。JPEG 块效应等低频压缩噪例外
# ——升 σ 反而会被当结构拾取放大，此时降 amount 或关锐化
image_full_sharpen_sigma = 1.5

# 白边抑制曲线：折点 [shoot_v, ratio] 数组，折点间线性插值（隐含首点 [0,1]，
# 末点后平坦 = 末点 ratio）。shoot_v = 逐点 3×3/5×5/7×7 多尺度亮度方差按相机
# ISP shoot.c mode 0 整数组合出的强边标记（0~255）：shoot_v 大 = 孤立强边
# （7×7 窗口方差显著高于局部 3×3）——unsharp 白边亮晕的来源；细纹理各尺度
# 方差相近 shoot_v≈0。正向增强乘 ratio（只压亮侧 d>0 白晕），暗侧压暗全额
# 保留——取代旧 ±20 过冲钳制（钳差值连强边锐感一起削平）。
# 曲线语义：shoot_v 小时 ratio≈1（细纹理全额保留），到拐点后渐进压制——
# 比单斜率直线观感好（直线低段把弱边也压太多，实测弃用）。默认曲线（2026-
# 11-18 用户 release 实测定版）：
#   100 前几乎不动 → 150 降到 0.9 → 200 降到 0.6 → 240 降到 0.3 → 255 收
#   到 0.05（极强边保留 5% 锐感防钝化）；
# 更狠压白边改 [120,0.9],[180,0.5],[220,0.15],[255,0.0]；想完全压灭极强边把
# 末点改 [255, 0.0]；校验 x∈(0,255] 严格递增、y∈[0,1]，非法回退默认并 WARN；
# 空数组 [] = 关闭（连方差计算都跳过）
image_full_sharpen_white_edge_curve = [[100.0, 1.0], [150.0, 0.9], [200.0, 0.6], [240.0, 0.3], [255.0, 0.05]]

# shoot 敏感度增益（1~1024，默认 1.0 = 最接近原始 shoot.c 语义）：乘在跨尺度
# 方差差（v7−v3、v7−v5，方向保留不取 abs——abs 实测响应毛躁、结构紊乱已弃
# 用）上再组合 shoot_v。缓坡等边缘的方差差量级小、落在 0..255 低段——增益
# 把它线性展宽，让默认曲线拐点（150 起）对得上。弱边缘压不住时调大，典型
# 2~16；过大（>64）细纹理方差差也被放大，曲线低段开始误伤——配合曲线拐点
# 一起调
image_full_sharpen_white_edge_gain = 1.0

# shoot flank 带外扩半径（0~4，默认 1，0 = 关）：方向保留的方差差响应呈
# "高亮带在真边缘外侧 1~2px"——max 膨胀外扩（1 = 外扩 1px，2 = 外扩 2px）
# 覆盖真边缘线本身。dump 图显示的就是外扩后的值（所见即所得）
image_full_sharpen_white_edge_dilate = 1

# 锐化前保边降噪（压平坦区噪点不糊边缘）
image_full_denoise = true

# 降噪算法切换（两算法参数独立、用对应算法名前缀区分，strength 同量纲可
# 同值对比；非法值回退导向并 WARN）：
#   "guided"    = 导向滤波（默认）——按结构方差区分"平坦"与"渐变"（He et
#                 al. 2010 self-guided），噪点压掉而弱纹理/过渡带保留，
#                 锯齿感最弱、暗部渐变无分层；平坦区平均窗口更大（半径
#                 可配），时间与半径无关（O(1)/px 滑动均值，无 exp）
#   "bilateral" = 双边 3×3——按亮度差定权重，孤立强噪点抑制更狠、眼部
#                 更锐；但渐进弱纹理（皮肤渐变、头发过渡带）会被吸干
#                 （涂抹感、深放大时头发边缘呈阶梯状锯齿），大 σ 锐化下
#                 暗部投影还可能出现分层
image_full_denoise_algo = "guided"

# 双边降噪颜色容忍度 σc（0~100 亮度级，仅 algo = "bilateral" 生效）：亮度差
# 小于它的邻点参与平均。影响档位：
#   5~10 = 温和：只压最细噪点，毛孔/弱纹理基本全保，噪点有残留；
#   10~20 = 通用：平坦区噪点压净，皮肤渐变开始轻微涂抹；
#   20~50 = 激进：噪点（含中尺度斑块）压得净，弱纹理明显抹平；
#   >50 ≈ 普通模糊：绝大多数邻点参与平均，边缘也糊。
# 涂抹感重 → 调小；噪点残留 → 调大
image_full_bilateral_strength = 10.0

# 导向降噪结构容忍度（0~100 亮度级，仅 algo = "guided" 生效）：语义 = 局部
# 亮度差容忍 D，内部换算 ε = D²（方差阈值）——方差 ≪ ε 的区域（平坦/噪点）
# 被平均压噪，方差 ≫ ε 的区域（边缘/渐变）原值保留，皮肤渐变、头发过渡带
# 不被吸干。数值与双边 σc 同量纲，同值对比即可。影响档位（同等数值下涂抹感
# 轻于双边——判据是整窗方差而非点到点差）：
#   3~8 = 温和：只压细噪点，皮肤渐变/头发过渡带完整保留；
#   15~30 = 通用（高 ISO 重噪）：噪点幅度大，ε 提到 std 10~15 级才压得住；
#   30~60 = 激进（胶片颗粒/老照片）：中尺度斑块也压，弱纹理明显涂抹；
# 注意噪点本身抬高局部方差——重噪图需要更大 D 才压得住。
# 涂抹感重 → 调小；噪点残留 → 调大
image_full_guided_strength = 10

# 导向滤波窗口半径（1~8，仅 algo = "guided" 生效）。影响档位：
#   1（3×3）= 默认（人像实测定版）：刚好覆盖 1~2px 噪点自相关尺度，方差
#     估计最局部、弱纹理判据最敏锐；大窗口会把发丝间隙/毛孔的方差稀释
#     → 误判平坦 → 涂抹/锯齿；
#   2~4（5×5~9×9）= 重噪图（噪点成团/中尺度斑块）；
#   5~8 = 低频色斑也能压，但平坦区涂抹感明显、弱纹理发糊。
# 时间与半径无关（O(1)/px 滑动均值）
image_full_guided_radius = 1

# 防振铃（默认开）：Lanczos 放大时负瓣在强边两侧产生越出源邻域值域的过冲/
# 欠冲（Gibbs 振铃，深放大后白线/暗线）——放大输出像素 clamp 回源对应
# 区域的逐通道 min/max：域内合法信号（边缘过渡/细纹理/1px 亮线）恒等映射
# 不动、越界值必为插值伪像才被剪，锐度无损（mpv zscale antiring 同思路）。
# 与 shoot 白边抑制互补：shoot 治放大前 unsharp 白边，这个治放大时
# Lanczos 振铃。仅高清放大档生效；强边显"肉"或怀疑压细节时可关掉对比
image_full_antiring = true

# 对比视图 full 重采样使能（默认关）：多图对比下主栏深放大是否解码
# ×ppp×3 高密度纹理。对比的目的是"同屏看差异"，base 档纹理已够用，
# full 档主栏与 base 档副栏锐度不一致反而干扰判断，主栏切换还逐次触发
# full 解码——默认关。单图模式不受此键影响（恒允许升级）
image_compare_full_resample_enable = false

# ============ debug dump（默认关，debug 完关掉）============
# dump 文件落盘在缓存目录（[cache].cache_dir，"auto" = 系统临时目录下的
# archview_cache）的 debug/ 子目录，固定文件名覆盖。开启后每次高清档解码
# 都写一次（bmp 为原图分辨率 8bit 灰度图，几 MB 级），调试完请关闭。
# 全部为锐化算法调试用途（sharpen 前缀）。

# 锐化 dump shoot 边缘图：shoot_v（多尺度方差强边标记）直出灰度 bmp
# （debug/sharpen_shoot_dump.bmp）——白 = 强边（白边抑制生效区），黑 =
# 细纹理（锐度全额保留区）。用于验证边缘检测是否正确
image_full_sharpen_dump_shoot = false

# 锐化 dump 白边抑制曲线查表：实际使用的 shoot_v→ratio 插值结果 256 行
# （debug/sharpen_curve_dump.txt）——用于验证配置折点的分段线性插值是否
# 正确生效
image_full_sharpen_dump_curve = false

# 锐化 dump 前后 Y 通道：锐化输入 Y（降噪后）与锐化输出 Y 各存一张灰度
# bmp（debug/sharpen_y_before.bmp、debug/sharpen_y_after.bmp）——逐点
# 核对锐化在亮度上做了什么
image_full_sharpen_dump_y = false

# 锐化 dump shoot 白边抑制前后的增强量（差异值）：before = amount×d
# （未抑制），after = amount×d×ratio（抑制后），各存一张灰度 bmp
# （debug/sharpen_delta_before.bmp、debug/sharpen_delta_after.bmp）——
# 差异值有正有负，整体 +128 偏移（128 = 零点增强）；两图相减即白边抑制
# 实际削掉的量
image_full_sharpen_dump_enhance = false

# ============ 解压预取 ============
[extract]

# 下游预取上限（MB），0 = 关闭：solid 包（7z/solid RAR）解压时顺带预取后续文件
# （下标更大 = 列表向下 = 解码流推进方向）。一次顺序解压流解出一批文件，
# 翻页直接命中，摊薄固态包定位成本
prefetch_downstream_mb = 512

# 上游顺带落盘最大上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置条目
# （下标更小 = 列表向上；7z 限于目标所在 folder 内、RAR 为整个前缀）顺带
# 写盘，随机跳看后回看直接命中缓存。解码零增量，仅占磁盘写入量。
# 语义与下游不对称：下游 prefetch_downstream_mb 尽量取满，本键只是上限（max）
# ——实取多少取决于免费区（folder 前缀）大小与已缓存分布，通常小于该值
prefetch_upstream_max_mb = 512

# ============ VarTidy 整理器 ============
[vartidy]

# VarTidy（"archview.exe tidy" CLI 子命令）配置
# var 仓库根目录。必须已存在——整理器绝不修改/移动/删除仓库已有内容，
# 仅新建作者子目录并写入仓库没有的新 var。空 = 未配置（CLI 需 -o 指定）
repo_dir = ""

# 报告输出目录；"auto" = 首个输入压缩包所在目录（不写入仓库，保持仓库纯净）
report_dir = "auto"

# ============ 编码引擎 ============
[encode]

# 「编码为视频」（CLI archview encode / UI 右键「编码为视频…」）编码参数。
# vcodec 需本机 libmpv 构建内置；crf/preset 组装为 ovcopts 键值对，
# ovcopts 是附加编码器选项（如 "bf=3" / "threads=8"），同键覆盖 crf/preset；
# 引擎自动追加 bf=0 兜底（显式给 bf=/bframes= 除外）
vcodec = "libx264"

# 恒定质量（x264/x265 crf）：越低画质越高体积越大
crf = 23

# 编码速度预设（ultrafast…veryslow）
preset = "medium"

# 附加编码器选项；空 = 无
ovcopts = ""
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// 防漂移：带注释的默认模板解析回来必须与代码默认值逐项一致。
    /// 任何一侧改默认值都必须同步另一侧，本测试会拦住遗忘。
    /// 另设节头存在性断言：节头（如 [cache]）被误删时，键会全部落到顶层
    /// 被 serde 静默忽略、各 table 字段恰好以默认值兜底——上面的逐项比较
    /// 会假通过（实际发生过），节头缺失必须在这里先炸
    #[test]
    fn default_template_matches_config_default() {
        for head in [
            "[cache]",
            "[media]",
            "[io]",
            "[raw]",
            "[player]",
            "[image]",
            "[ui]",
            "[extract]",
            "[vartidy]",
            "[encode]",
        ] {
            assert!(
                DEFAULT_TOML.contains(&format!("\n{head}\n")),
                "DEFAULT_TOML 缺少节头 {head}（键会静默落到顶层被忽略）"
            );
        }
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

    /// UI 重绘频率三键：自定义值可解析；pacing_ms 越界钳制 5~240fps
    #[test]
    fn ui_redraw_fps_keys() {
        let cfg: Config = toml::from_str(
            "[ui]
redraw_hover_fps = 20
redraw_drag_fps = 144
redraw_other_fps = 0",
        )
        .unwrap();
        assert_eq!(cfg.ui.redraw_hover_fps, 20);
        assert_eq!(cfg.ui.redraw_drag_fps, 144);
        assert_eq!(cfg.ui.redraw_other_fps, 0); // 解析不钳制，取间隔时才钳
        assert_eq!(UiConfig::pacing_ms(cfg.ui.redraw_hover_fps), 50);
        assert_eq!(UiConfig::pacing_ms(cfg.ui.redraw_drag_fps), 7); // 1000/144 ≈ 6.9
        assert_eq!(UiConfig::pacing_ms(0), 200); // 0 → 钳 5fps → 200ms
        assert_eq!(UiConfig::pacing_ms(999), 4); // 999 → 钳 240fps → ~4ms
        assert_eq!(UiConfig::pacing_ms(60), 17); // 1000/60 ≈ 16.7 → 17
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
        // 整节补全：[raw] 连注释带内置五条预设一并补上（老用户升级场景）
        assert!(out.contains("# [[raw.demux]]"));
        assert!(out.contains("bayer_auto16le"));
        assert!(out.contains("image_extensions"));
        // 根级新键必须在第一个 [表] 头之前（否则会归属到最后一个表）
        let key_pos = out.find("log_level").expect("log_level 已补全");
        let tbl_pos = out.find("\n[").expect("存在表头");
        assert!(key_pos < tbl_pos, "根级补全键应出现在首个表头之前");
        // 补全结果语义正确：用户值优先 + 缺失项取默认
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.codepage, "big5");
        assert_eq!(cfg.ui.slideshow_secs, 2.0);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.image.image_max_dim, 4096);
        // 幂等：再跑一遍补全 0 项，输出不变
        let mut doc2: DocumentMut = out.parse().unwrap();
        assert_eq!(backfill_table(doc2.as_table_mut(), def.as_table()), 0);
        assert_eq!(doc2.to_string(), out);
    }

    /// [[raw.demux]] 解析与路由：多条预设按声明序首命中（具体在前、
    /// 宽泛在后）；省略 fps 取默认 3.0、省略 bit_shift 取默认 0；
    /// 路由时剥离路径分隔符
    #[test]
    fn raw_demux_parse_and_route() {
        let cfg: Config = toml::from_str(
            r#"
[raw]
[[raw.demux]]
match = "sensor_a_*.bin"
pixfmt = "bayer_rggb16le"
width = 1920
height = 1080
fps = 30.0
bit_shift = 4

[[raw.demux]]
match = "*.bin"
pixfmt = "gray"
width = 640
height = 480
"#,
        )
        .unwrap();
        assert_eq!(cfg.raw.demux.len(), 2);
        // 首命中：sensor_a_001.bin 命中第 1 条（具体）而非第 2 条（宽泛）
        let hit = cfg.raw_demux("sensor_a_001.bin").expect("首条命中");
        assert_eq!(hit.pixfmt, "bayer_rggb16le");
        assert_eq!((hit.width, hit.height), (1920, 1080));
        assert_eq!(hit.fps, 30.0);
        assert_eq!(hit.bit_shift, 4, "bit_shift 显式解析");
        // 其余 .bin 落到宽泛兜底；fps/bit_shift 省略 → 默认 3.0 / 0
        let fallback = cfg.raw_demux("cap_003.bin").expect("宽泛兜底命中");
        assert_eq!(fallback.pixfmt, "gray");
        assert_eq!(fallback.fps, 3.0);
        assert_eq!(fallback.bit_shift, 0, "bit_shift 省略默认 0（不位移）");
        // glob 大小写不敏感 & 路径剥离（预设写文件名，传全路径也能命中）
        assert!(cfg.raw_demux("dir/SENSOR_A_9.BIN").is_some());
        assert!(cfg.raw_demux(r"C:\cap\cap_003.bin").is_some());
        // 无命中 → None；默认五条预设不覆盖 .bin → 普通数据文件不可见
        assert!(cfg.raw_demux("note.txt").is_none());
        assert!(Config::default().raw_demux("a.bin").is_none());
    }

    /// 默认模板内置五条 FPGA dump 预设（nv12/yuv400/yuv420/rgb/raw）：
    /// 宽高 0 交由文件名内嵌 WxH 兜底；raw 用伪格式 bayer_auto16le + bit_shift 4
    #[test]
    fn raw_default_presets_builtin() {
        let cfg = Config::default();
        assert_eq!(cfg.raw.demux.len(), 5);
        let nv12 = cfg.raw_demux("cap_01.NV12").unwrap();
        assert_eq!(
            (nv12.pixfmt.as_str(), nv12.width, nv12.height),
            ("nv12", 0, 0)
        );
        assert_eq!(nv12.fps, 3.0);
        let y400 = cfg.raw_demux("x.yuv400").unwrap();
        assert_eq!(y400.pixfmt, "gray");
        let y420 = cfg.raw_demux("x.yuv420").unwrap();
        assert_eq!(y420.pixfmt, "yuv420p");
        let rgb = cfg.raw_demux("frame_640x480.rgb").unwrap();
        assert_eq!(rgb.pixfmt, "rgb24");
        let raw = cfg.raw_demux("fpga_in_s0_1920x1080_00_1.raw").unwrap();
        assert_eq!(raw.pixfmt, "bayer_auto16le");
        assert_eq!(raw.bit_shift, 4);
        // 伪格式按 16bit 容器参与整除校验；仅 le 变体
        assert_eq!(raw_pixfmt_bits("bayer_auto16le"), Some(16));
        assert_eq!(raw_pixfmt_bits("BAYER_AUTO16LE"), Some(16));
        assert_eq!(raw_pixfmt_bits("bayer_auto16be"), None);
        // 可见性：默认五条后缀进视频桶；其它非标后缀仍不可见
        assert!(cfg.is_video_ext("fpga_in_s0_1920x1080_00_1.raw"));
        assert!(cfg.is_video_ext("x.nv12"));
        assert!(cfg.is_video_ext("x.yuv400"));
        assert!(cfg.is_video_ext("x.yuv420"));
        assert!(cfg.is_video_ext("x.rgb"));
        assert!(!cfg.is_video_ext("a.yuv"));
        assert!(!cfg.is_video_ext("cap.bin"));
    }

    /// 文件名内嵌 WxH 解析：标准 dump 名、大写 X、多候选按出现序；
    /// 边界规则防误匹配（字母/数字邻接、十六进制 0x10 因 W=0 被拒、超长段）
    #[test]
    fn parse_filename_dims_cases() {
        assert_eq!(
            parse_filename_dims("fpga_in_s0_1920x1080_00_12345.raw"),
            vec![(1920, 1080)]
        );
        assert_eq!(
            parse_filename_dims("cap_1280X720_01.nv12"),
            vec![(1280, 720)]
        );
        assert_eq!(
            parse_filename_dims("a_640x480_b_320x240.yuv"),
            vec![(640, 480), (320, 240)]
        );
        assert!(parse_filename_dims("v2x4raw.bin").is_empty(), "左侧邻字母");
        assert!(parse_filename_dims("mp3x4.bin").is_empty(), "左侧邻字母");
        assert!(parse_filename_dims("raw4x8x.bin").is_empty(), "右侧邻字母");
        assert!(
            parse_filename_dims("addr_0x10.bin").is_empty(),
            "W=0 十六进制"
        );
        assert!(parse_filename_dims("no_dims.bin").is_empty());
        assert!(
            parse_filename_dims("f_123456x789.bin").is_empty(),
            "段长 >5 位"
        );
    }

    /// raw 可见性路由：预设即真相——[[raw.demux]] glob 命中才归视频桶，
    /// 未命中的非标扩展名（.yuv/.bin）一律不可见；普通视频/图片不受影响
    #[test]
    fn is_video_ext_raw_visibility() {
        let cfg = Config::default();
        // 原有行为不变
        assert!(cfg.is_video_ext("movie.mp4"));
        assert!(!cfg.is_video_ext("photo.jpg"));
        assert!(!cfg.is_image_ext("frame.yuv"));
        // 无任何预设时非标扩展名一律不可见（不猜格式，防 mpv 盲试黑屏）
        assert!(!cfg.is_video_ext("a.yuv"));
        assert!(!cfg.is_video_ext("cap.bin"));
        // 配预设后：命中 → 可见（预设即参数来源）
        let cfg2: Config = toml::from_str(
            r#"
[raw]
[[raw.demux]]
match = "cap*.bin"
pixfmt = "nv12"
width = 320
height = 240
"#,
        )
        .unwrap();
        assert!(cfg2.is_video_ext("cap0001.bin"));
        // 预设未命中的 .bin 依旧不可见——防污染文件列表
        assert!(!cfg2.is_video_ext("other.bin"));
        // .yuv 同样需显式声明（如 match = "*.yuv"），无默认扩展名捷径
        assert!(!cfg2.is_video_ext("a.yuv"));
    }

    /// 极简 glob：`*` 任意序列（含空）、`?` 恰一字符、大小写不敏感、
    /// 无通配符精确匹配、多字节字符按字符处理
    #[test]
    fn glob_matcher() {
        // 无通配符：精确匹配（大小写不敏感）
        assert!(glob_match("cap.bin", "CAP.BIN"));
        assert!(!glob_match("cap.bin", "xap.bin"));
        // * 任意长度（含空）；? 恰一字符
        assert!(glob_match("*.bin", "a.bin"));
        assert!(glob_match("sensor_*.yuv", "sensor_.yuv")); // * 可匹配空
        assert!(!glob_match("sensor_*.yuv", "sensor_1.bin"));
        assert!(glob_match("sensor_?.yuv", "sensor_7.yuv"));
        assert!(!glob_match("sensor_?.yuv", "sensor_12.yuv"));
        assert!(glob_match("sensor_?x*.raw", "sensor_1xyz.raw"));
        // 经典回溯：* 吞一字符后重试对齐
        assert!(glob_match("*aab", "aaab"));
        assert!(!glob_match("*aab", "aaxb"));
        // 多字节字符按字符匹配（不按字节截断）
        assert!(glob_match("传感*.bin", "传感器A.bin"));
    }

    /// 字节匹配实现的字符粒度回归：`?` 与 `*` 回溯均按 UTF-8 字符推进
    /// （一个三字节汉字 != 三个 `?`），与旧 Vec<char> 实现语义一致
    #[test]
    fn glob_matcher_multibyte_granularity() {
        // ? 恰一字符（汉字按 1 个字符计，不按 3 字节）
        assert!(glob_match("a?c.bin", "a中c.bin"));
        assert!(!glob_match("a??c.bin", "a中c.bin"));
        assert!(!glob_match("a?c.bin", "a中文c.bin"));
        // * 回溯按字符吞（吞「传感器」整段后对齐 A）
        assert!(glob_match("*A*.bin", "传感器A.bin"));
        assert!(glob_match("传感?A.bin", "传感器A.bin"));
        assert!(glob_match("传感*器*.bin", "传感器A.bin"));
    }

    /// 后缀快路分流：suffix_pattern 形状判定边界 + 快路（ends_with）与
    /// 慢路（glob 回溯）对同一批名字结果逐点等价
    #[test]
    fn suffix_fast_path_equivalence() {
        // 形状判定：* 仅在开头 → 后缀形；中置 */?、无通配 → None 走 glob
        assert_eq!(suffix_pattern("*.raw"), Some(".raw"));
        assert_eq!(suffix_pattern("*_dump.raw"), Some("_dump.raw"));
        assert_eq!(suffix_pattern("*"), Some(""));
        assert_eq!(suffix_pattern("*.nv12"), Some(".nv12"));
        assert_eq!(suffix_pattern("sensor_a_*.bin"), None);
        assert_eq!(suffix_pattern("*_?aaa*.raw"), None);
        assert_eq!(suffix_pattern("*a*b.raw"), None);
        assert_eq!(suffix_pattern("cap.bin"), None);

        // 快慢两路等价：覆盖命中/未命中/边界（空名、仅后缀、超集后缀）/大小写/中文
        let names = [
            "a.raw",
            "A.RAW",
            "x_dump.raw",
            "raw",
            ".raw",
            "a.raws",
            "sraw",
            "传感器.raw",
            "",
        ];
        for pat in ["*.raw", "*.RAW", "*_dump.raw", "*", "*传感器.raw"] {
            for name in names {
                let fast = match suffix_pattern(pat) {
                    Some(s) => {
                        let (f, s) = (name.as_bytes(), s.as_bytes());
                        f.len() >= s.len() && f[f.len() - s.len()..].eq_ignore_ascii_case(s)
                    }
                    None => unreachable!("测试用例应为后缀形: {pat}"),
                };
                assert_eq!(fast, glob_match(pat, name), "pattern={pat} name={name}");
            }
        }
    }

    /// RAWVIDEO_PIXFMTS 完整性：数量快照、bayer 12 项、关键 bits 抽查、
    /// 容器位数口径、大小写不敏感、MIPI/调色板不在名单、无重复项
    #[test]
    fn raw_pixfmt_table() {
        // 数量快照：增删格式须显式改此数（与 ffmpeg -pix_fmts 筛选名单对账）
        assert_eq!(RAWVIDEO_PIXFMTS.len(), 195);
        // bayer 恰 12 项（8bit×4 + 16le/be×4）
        assert_eq!(
            RAWVIDEO_PIXFMTS
                .iter()
                .filter(|(n, _)| n.starts_with("bayer_"))
                .count(),
            12
        );
        // 关键格式抽查（8/16-bit 与 bayer 两套口径一致）
        assert_eq!(raw_pixfmt_bits("yuv420p"), Some(12));
        assert_eq!(raw_pixfmt_bits("nv12"), Some(12));
        assert_eq!(raw_pixfmt_bits("gray"), Some(8));
        assert_eq!(raw_pixfmt_bits("rgb24"), Some(24));
        // 容器位数口径：9~16bit 采样占 2 字节容器（有意取容器位而非
        // 逻辑位，帧字节数校验才与文件实际大小吻合）
        assert_eq!(raw_pixfmt_bits("yuv420p10le"), Some(24));
        assert_eq!(raw_pixfmt_bits("gray10le"), Some(16));
        assert_eq!(raw_pixfmt_bits("rgb555le"), Some(16));
        // 查表大小写不敏感
        assert_eq!(raw_pixfmt_bits("YUV420P"), Some(12));
        // 不支持的格式：MIPI packed bayer / 调色板 / 杜撰名
        assert_eq!(raw_pixfmt_bits("mipi_rggb10"), None);
        assert_eq!(raw_pixfmt_bits("pal8"), None);
        assert_eq!(raw_pixfmt_bits("notafmt"), None);
        // 名单无重复项（防手滑双写同格式不同 bits）
        let mut names: Vec<&str> = RAWVIDEO_PIXFMTS.iter().map(|(n, _)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "RAWVIDEO_PIXFMTS 存在重复项");
    }
}
