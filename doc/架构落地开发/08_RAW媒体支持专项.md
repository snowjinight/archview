> [返回目录](README.md)

## 8. RAW 媒体支持专项（2026-11-01：裸数据 dump + 相机 RAW 预览）

> 机制与链路见 [ARCHITECTURE §13.6](../架构设计/13_渲染架构迁移.md)（dump 播放 / 可见性路由 / 失败回显）与
> [§13.4](../架构设计/13_渲染架构迁移.md)「相机 RAW 照片预览」段；本节记实现踩坑与测试方法细节。

### 8.1 名单计数：手数 194，实测 195（快照断言先跑再定值）

**现象**：`RAWVIDEO_PIXFMTS` 从 `ffmpeg -pix_fmts` 输出转录后，数量快照断言
手数写 194，跑测试实得 195。

**根因**：195 行长名单手工计数漏 1 项；分组注释里的"（N）"小计与实际行数
不同步，肉眼逐组对账难定位。

**修复/教训**：快照类断言（len / 数量 / 字节数）**先跑一次让失败信息报出
实际值，再回填常量**，禁止手数；随后按组 grep 行数核对小计（Bayer 12 /
Gray 16 / YUV planar 14+56 / NV 23 / packed 10 / RGB 26+12 / GBR 24 /
XYZ 2 = 195）。[README](../../README.md)「raw 格式表」附录同表转写时同样按组对账。

### 8.2 image 0.25 imageops 自由函数签名（EXIF 方向矫正）

**现象**：`apply_orientation` 调 `imageops::rotate90(&img)` 类型不合——
期望操作 `DynamicImage` 并保持返回类型。

**根因**：image 0.25 的 `imageops::rotate90 / rotate270 / flip_horizontal`
等**自由函数接收 `&ImageBuffer` 且返回 `ImageBuffer`**（非泛型 I、非
DynamicImage），与 DynamicImage 直接混用即编译错。

**修复**：`to_rgba8()` 统一转 ImageBuffer → imageops 自由函数 →
`DynamicImage::ImageRgba8(..)` 包回；镜像/180° 用 `*_in_place` 变体零分配。
EXIF 恰 8 个值，`raw_files::Orientation` 穷尽匹配无兜底；EXIF 5/7 = 旋转后
镜像（主/反对角线转置），测试用 2×1 红绿条的像素方位把 8 个映射全部钉死。

**附带**：测试里取 `img.width()/height()` 需 `use image::GenericImageView`
（trait 方法，不在 DynamicImage 固有方法里），否则"找不到方法"。

### 8.3 e2e 三连坑：END_FILE 通道的真实行为（player/mod.rs 集成测试）

背景：`e2e_rawvideo_dump_with_demux_opts` 第二段要验证「装载失败 →
END_FILE(ERROR) + error 级日志行」回显链路（[ARCHITECTURE §13.6](../架构设计/13_渲染架构迁移.md)）。
**PlayerService::new 需要 glow::Context（GL 上下文），单测环境拿不到** →
用裸 `mpv_ffi` ctx（vo/ao=null、keep-open=always）手动复现泵线程的日志订阅
（`request_log_messages(ctx, "error")`）与事件解析。三轮失败、三个独立的坑：

**坑 A：`loadfile replace` 先给被替换旧文件发 END_FILE(STOP) 伪事件。**
断言 `reason == ERROR(4)` 实得 2（STOP）——第一段暂停中的 nv12 被替换时
先收到它自己的 END_FILE。修复：事件循环加 `seen_start` 守卫——先见
START_FILE、再遇 END_FILE 时跳过 `reason == STOP(2)`。

**坑 B：mipi_rggb10 是真格式（把"我以为的假格式"当非法值用）。** 原计划
（方案 §7.4 spike）用 mipi_rggb10 当"非法 pixfmt"触发装载失败——本机系统
ffmpeg `-pix_fmts` 查无此名，想当然认为非法。实测 libmpv **自带的更新版
ffmpeg（≥5.0，收录 MIPI packed Bayer）认识该名**：选项合法、装载成功；
且成功装载的文件因 pause 粘滞（keep-open + 上一段的暂停）永不结束 → 15s
超时。修复：换真正不存在的 `no_such_pixfmt` + loadfile 后显式 `pause=no`
（pause 是**全局属性**，跨 loadfile replace 粘滞到新文件——
PlayerService::load 同款处理）。

**坑 C：选项解析失败只发 START_FILE + 日志，不发 END_FILE。** no_such_pixfmt
后仍超时；把 trace（每条 END_FILE 的 reason 列表）与 err_log 注入 assert
消息后一眼定位：`trace=["END_FILE(reason=2)"] err_log="Error parsing option
demuxer-rawvideo-mp-format"`——mpv 对**装载选项解析失败**（per-file options
坏值）根本不发 END_FILE（本 libmpv 实测）。修复：改装载不存在的文件触发
**文件级错误**（END_FILE reason=4、error=-13、log="Failed to open..."）——
这恰是生产真实失败源：UI 层 raw_demux_opts 前置校验后，选项坏值路径根本
到不了 mpv。

**教训**：①外部库行为边界（事件何时发/不发）必须 e2e 实测钉死，文档与
直觉都不可信；②测试素材选"确定不存在"的值（no_such_file.nv12），不要选
"我认为不存在"的值（mipi_rggb10）——版本差异随时打脸；③超时类断言必须把
现场证据（事件 trace / 日志原文 / 状态标志）注入 assert 消息，否则每轮
失败都要重新加打印排查。Rust 细节：`let end_file;` 循环内定赋 + break 过
unused_assignments 警告（超时 assert 是 panic 发散路径，定赋分析认可）。

### 8.4 流程坑复发：同文件并行编辑被静默吞（[§2](02_渲染迁移与图片浏览器踩坑.md) 坑 10）

本轮多处文档编辑再次验证：同一文件的两个 SearchReplace 并行下发，只有一个
落盘、另一个报成功实未写入。规则不变：**同文件编辑必须串行**，编辑后 Read
回验关键行。

### 8.5 需求变更：raw.extensions 移除——预设即唯一入口（2026-11-02）

初版设计里裸数据可见性是两条路：`[raw] extensions` 扩展名清单（默认
yuv/nv12）命中即进视频列表 + `[[raw.demux]]` 预设命中即见且可播，清单命中
但无预设时前置报错引导配置（`raw_needs_preset`）。上线即被用户一句话点破
冗余：**写清单根本省不掉预设**——.nv12 文件就算在 extensions 里，仍必须在
`[[raw.demux]]` 写 `*.nv12` + pixfmt + 宽高才能播；两处都写、参数却只有
预设里有，清单只贡献"可见但不可播"的中间态（用户会先看到列表项、点开才报
错配置缺失，体验更差而非更好）。

修订（一处真相）：删 `RawConfig::extensions` / `raw_visible` /
`raw_needs_preset`，`is_video_ext = video_extensions 命中 ∨ raw_demux(name)`
——预设 glob 是裸数据唯一入口，命中即可见可播，未写预设的 .yuv/.bin 干脆
不出现。连带删除校验第④分支（"命中清单但无预设"随清单消亡，四分支→三
分支）；用户手动加进 `media.video_extensions` 的非标文件回落为普通装载，
失败走 END_FILE 通道回显（不再前置拦截——那类文件本来就不在裸数据语义里）。

兼容与模板：serde 忽略未知字段，老 config 里残留的 `extensions = [...]`
静默跳过无需迁移；`[raw]` 节只剩注释时 backfill 照常整节补全（toml_edit
decor 保留）。同时按用户要求在 config 模板 `[raw]` 注释里加常用 pixfmt
速查（yuv420p/nv12/nv21/yuvj420p、yuv420p10le/p010le/yuv420p16le、
4 个 bayer 16le）——195 项全名单查 README 附录太远，开箱即写的高频子集
放模板内一行可达。

教训（适用于一切"清单 + 规则"双层设计）：**两套机制给同一文件两种命运时，
先问较弱的那套是否真能独立成立**。extensions 离开预设不能播，预设离开
extensions 却完全自洽（glob 想写 *.yuv 就写）——不对称即冗余信号，
该删的是弱侧。

### 8.6 Bug 修复：目录模式收集层旁路 raw 预设（2026-11-02）

§8.5 移除 raw.extensions 当天即暴露连带 bug：用户写 `[[raw.demux]]
match = "*.yuv420"` 后打开 .yuv420 所在目录，文件既不出现也不能播放。
config 写法、UI 过滤层（`rebuild_view` 走 `is_video_ext`，含预设 glob）均
正确——真因在第三层：目录模式的收集层 `DirVFS::open` 收的是**扩展名白名单**
（`&[String]` 逐项比对，open.rs 拼接 video+image extensions 传入），预设
glob 命中对它无效，非标名在扫描阶段即被丢弃，根本轮不到列表过滤。

三层梳理：压缩包模式列出全部条目、过滤全在 rebuild_view（预设生效，无恙）；
目录模式 = 收集（DirVFS 白名单，预设失效）+ 过滤（rebuild_view，预设生效）
——两层各信一套真相，弱侧（白名单）悄悄把文件丢了。

修复（一处真相，两处消费）：`DirVFS::open/walk` 的 `exts: &[String]` 改为
泛型名字谓词 `F: Fn(&str) -> bool`；生产侧（open.rs 目录分支）传
`cfg.is_video_ext(name) || cfg.is_image_ext(name)`（Config 已 derive
Clone，clone 入线程闭包）——收集与过滤同源，预设 glob 在收集层即生效。
测试调用点全量适配（player e2e、ui tests 加 `media_pred()` 辅助，共 5 处
——首改漏了第 4 处白名单调用，编译期 E0277 拦下）+ 新增回归测试
`name_predicate_collects_raw_preset_glob_names`（非标名进集合、非媒体名
排除）。98 passed。

教训：**"过滤规则"一旦有多处消费，任何一处自己另写一套判定都是旁路**。
扩展名白名单在 raw 支持之前就存在且自洽（标准媒体确实按扩展名判），新增
预设入口时只改了 `is_video_ext` 却漏查它的全部语义等价消费方——白名单
恰好是 `is_video_ext` 的子集，标准文件两套判定同结果，测试全绿掩盖了
缺口。改判定函数时，grep 它的语义等价物（这里是 `media_exts` 拼接）比
改函数本身更重要。

### 8.7 需求变更：三态播放模式 + 顶栏消息单槽覆盖（2026-11-02）

非标 RAW 播放支持（§8.5/§8.6）引入的连带需求变更：裸数据 dump 播完
"停末帧供检查"的用法被用户推广为默认播放行为，顺带把顶栏消息机制
（红/绿）一起定稿。机制与语义表见 [ARCHITECTURE §14.2](../架构设计/14_底部控制栏布局.md)。

**播放模式：单帧门控方案被否 → 通用三态（默认定格）**

最初候选是"两态布尔 + 单帧门控"（视频/图片各自硬编码播完行为），讨论
中被否：门控只在单一帧事件上生效，"播完之后做什么"（推进/停住/循环）
三类语义塞不进布尔，每加一种行为都要再开特例。定为通用三态
`PlayMode { Hold, List, Loop }`，**默认 Hold（定格，播完停末帧）是用户
明确指定的**。循环顺序 Hold → List → Loop → Hold（从默认态出发逐级）。

实现上 Hold 零成本：全局 `keep-open=always` 本就保证 EOF 末帧定格 +
粘滞暂停，Hold 只需在 `video_eof_tick` 门控早退——且在读取 eof 之前
return（不写 `eof_handled`），Hold→List 切换时若正粘滞末帧立即推进；
Loop 复用 `loop-file=inf`（EOF 恒不触发），仅切到 Loop 时对已发生的
EOF 手动 `seek(0)` + 解除暂停复活（loop-file 不回溯 EOF）。旧两态
残留（`video_loop: bool`/`toggle_video_loop`）全量清除（grep 零残留）。

**顶栏消息：红绿超时方案两次被否 → 单槽覆盖 + 任意点击清绿**

旧机制是 notice 5 秒超时自动消失 + error 常驻。超时方案两次被否：
第一次（绿提示 5 秒超时，现行为）——用户不在场/在读列表时提示悄悄
消失，等于没提示；第二次（红也加超时防"红字钉死顶栏"）——错误被
时间冲掉后用户无从知道上次操作为何失败。定稿为**互斥单槽覆盖，彻底
无超时**：`set_err` 清绿写红、`set_notice` 清红写绿。绿的清除路径 =
任意鼠标点击（帧首 `any_click` 检测——用户操作了=看过了，无需逐按钮
判断）；红的清除路径仅两条：被新消息覆盖、成功播放新内容
（`play`/`play_image`/`load_extracted` 入口与切换内容源清双槽——后者
是用户动机"打开正常文件应清掉上个坏文件的红字"，否则红字跨包残留
会被误会成"这个正常文件也有问题"）。

新增 6 个回归测试（三态循环顺序 / eof_tick 门控边界 / 互斥覆盖 /
播放入口清槽 / 同帧 click 注入清绿不清红 / 内容源切换清槽+空集合
提示保留），104 passed（基线 98）。同帧 click 注入要点：egui 判定
click = press+release 同 pos 且间隔小于 max_click_duration——测试里
一帧注入 pressed:true + pressed:false 两个事件即可被 `any_click`
观测到。

教训：**"播完之后做什么"和"消息显示多久"都不是 UI 特例而是语义本体**——
前者抽象成枚举后每处消费（eof_tick/按钮/快捷键/tooltip）共享同一真相，
后者定为单槽+显式清除后，所有生产者只管写、清除路径可枚举（覆盖/翻篇/
点击），不再有"超时恰好发生在用户抬头前"这类不可测状态。

追记（同日）：Hold 图标 ❚❚ 与左侧暂停键 "||" 相邻撞形，被用户指为"像
暂停"。改 ▶❚（剪辑软件"播到出点"隐喻：播放 → 终点竖线停住），用户可
见文案由「定格播放」改「单文件播放（播完停在末帧）」——术语 Hold/定格
保留于代码与文档，仅图标与文案更直观。字形选形经 [§7.6](07_UI控制栏布局专项.md) 第三轮两轮实测：
❚ 上飘 1.5px → ▮ 同心但太宽 → 终选细线 I（顶缘与 ▶ 齐平），定稿 ▶I。

### 8.8 bayer16 低对齐发黑：bit_shift 流层修正（仅快路径，2026-11-02）

3 帧 bayer_bggr16le（640×360，fps=1）实测"画面极暗"：用户数据是
12bit 有效位 LSB 对齐（低对齐补零）存进 16bit 容器，而 FFmpeg swscale
的 bayer16→RGB 转换写死取高 8 位（`>>BAYER_SHIFT(8)`）——低对齐数据高
4 位恒零，只剩 16 级灰阶（≈6% 亮度）。顺带证伪 `bayer_*12le`：FFmpeg
bayer 只有 8/16 位两档，libmpv-2.dll 明文字符串与 195 项 pixfmt 名单
交叉验证均无此名（"12bit unpacked"格式并不存在）。mpv 末帧 PTS 语义
另解一惑：3 帧 fps=1 时长显示 0:03、播到 0:02 停属正常（末帧定格时刻
= (N-1)/fps，非 bug）。

**方案选择**：候选 A（解码后自转 RGB）与 B（播放器流层位移）中选 B——
不新增渲染链路，只对流经字节做 u16 左移。落点在 zvf:// 流层（open_cb
包装 ShiftReader），经 URI 查询串 `?s=N` 下发：mpv stream_cb 把完整 URI
原样交给 open_cb，不同 shift → 不同 URI → mpv 缓存天然隔离（同一
entry 切 shift 不会命中旧缓存字节）。

**ShiftReader 的唯一真坑：绝对偏移配对**。mpv read 回调的 buf 起点可以
是任意文件偏移（seek 后任意读长），若按 buf 内相对 0 起配对 u16，同一
像素会随读取切分漂移。解法：自跟踪逻辑偏移 pos，每次从 `pos & !1` 起读，
窗口首带 1 字节回看补配对、尾带 1 字节前看凑整对，移位后裁回
[pos, pos+n)。测试用逐 1 字节恶意切分 + 奇地址 seek 三基准钉死。
`wrapping_shl` 高位溢出丢弃正是期望语义（低对齐补的零移出去）。

**范围收窄 → 定案不做（用户决策）**：慢路径（7z/RAR 落盘 load_path）
不支持 bit_shift——shift 静默忽略、保持原行为。曾评估三候选：FileVFS+
zvf 单文件装载（须给 stream_cb 增设注册通道）/ 落盘后原地变换 / 解压流
中变换写入（后两者改写解压缓存，忠实拷贝语义丢失，shift 改配置后二次
位移、须把 shift 编进缓存键）。结论：bayer 位移适用面窄（仅低对齐
16bit 容器 bayer dump），收益不抵代码与架构复杂度代价，定案仅快路径
支持（记录于 [ARCHITECTURE §13.6](../架构设计/13_渲染架构迁移.md)）。

> **追记（2026-11-18，[§8.15](#815-需求变更慢路径-bit_shift-补齐--新增-yuv420rgb-通用预设-2026-11-18)）**：
> 上述定案已废止——重审发现否决 FileVFS 候选的关键前提（"须给 stream_cb
> 增设注册通道"）不成立：`register_with_progress` 本就接受任意 ArchiveVFS，
> 只缺一个单文件 VFS 实现（~50 行）。慢路径经 SingleFileVFS 借道 zvf://
> 已补齐，缓存忠实拷贝语义零破坏（当年否决后两候选的理由依然成立）。

**坑 10 三犯**：4 处 SearchReplace 并行提交同一文件，互相覆盖仅存活
1 处（[§2](02_渲染迁移与图片浏览器踩坑.md) 坑 10 / §8.4 已两次记录）——同文件多处编辑必须逐条串行提交，
改后 grep 复核存活数。

109 passed（基线 104）：新增 stream_cb 5 测（URI 往返+严格解析 / 全量
读 / 逐字节恶意切分 / 奇偏移+seek / 奇尾字节透传），既有 ui 校验测试
扩 3 用例（shift 越界 / 非 le 拒绝 / 合法组合带出 bit_shift）。

### 8.9 需求变更：文件名分辨率兜底 + 内置默认三预设（2026-11-09）

**动机**：FPGA dump 文件名自带分辨率（`sprintf("fpga_in_s%d_%dx%d_%02d_%lu.raw", ...)` →
`fpga_in_s0_1920x1080_00_12345.raw`），配置写错分辨率或懒得写时应有兜底；
且 nv12 / yuv400 / raw 三类常用后缀每次都要手写预设太重。

**实现**（设计语义见 [ARCHITECTURE §13.6](../架构设计/13_渲染架构迁移.md)）：

- `config.rs` 新增 `parse_filename_dims(name)`：**字节级**扫描 `WxH` 字样
  （`x`/`X` 分隔），边界规则——数字段长 1~5 位、段两端不邻接字母数字
  （避免把 `_00_`、`s0` 里的数字误判进分辨率），w/h>0 才入候选，按出现
  顺序返回。不引 regex，纯闭包实现。
- `raw_demux_opts` 宽高候选链：预设 width/height 非 0 在前 + 文件名兜底
  去重，逐候选过整除校验取首个通过者；候选空报「宽高无法确定（需预设
  或文件名 WxH）」，全失败报「与任一候选分辨率不整除（列出全部候选）」。
- `RawConfig` 移除 derive(Default) 改手写：内置三条默认预设 `*.nv12`→
  nv12、`*.yuv400`→gray、`*.raw`→bayer_auto16le+bit_shift=4（fps 均
  `default_raw_fps()`）；`RawDemux` derive Default，width/height 加
  `#[serde(default)]`（省略即 0，语义与填 0 一致走兜底）。
- DEFAULT_TOML 的 `[raw]` 节由注释示例改为三条真实条目 + 注释说明；
  防漂移测试同步。
- `RawLoad` 由 `opts: String` 重构为结构化字段（w/h/pixfmt/fps/bit_shift/
  auto_bayer）+ `opts()` 方法，为 bayer 探测（8.10）让出装载前改参的能力。

**测试**：新增 `raw_default_presets_builtin` / `parse_filename_dims_cases`
（含 `x` 分隔、边界邻接字母数字拒绝、多候选按序）；ui 校验用例扩 4
（文件名兜底成功 / 配错分辨率被文件名救回 / 预设有效优先 / auto_bayer
带出），改写 2（宽高无法确定与不整除的新文案）。全量 149 passed。

**两个实现级坑**：

1. 手写 Default 里 `..Default::default()` 会让 fps 落到 RawDemux derive
   的 0.0 而非 `default_raw_fps()`——三条预设必须显式写 `fps:`；
2. **默认预设与 serde 替换语义撞车**：用户 TOML 写了任何 `[[raw.demux]]`
   即整体替换默认数组（`#[serde(default)]` 不做逐项合并，只有顶层
   backfill 键级合并），但**程序内 push 会叠加**——encode CLI 测试
   `nv12_cfg()` 向 `Config::default()` push `*.nv12` 后，内置同名预设
   （宽高 0）按声明序抢先命中，整除校验直接失败。修复：`nv12_cfg()`
   先 `cfg.raw.demux.clear()` 再 push，并注释说明替换语义。

### 8.10 bayer_auto16le：Bayer 排列自动探测（伪格式，2026-11-09）

**动机**：raw dump 大多是 16bit unpack 的 Bayer（12bit 低对齐存 16bit
容器，bit_shift=4），但排列（RGGB/BGGB/GBRG/GRBG）无从得知——写错排列
红蓝互换。用户确认可用相关性启发式探测。

**实现**（算法机制见 [ARCHITECTURE §13.6](../架构设计/13_渲染架构迁移.md)）：

- 伪格式 `bayer_auto16le`：`raw_pixfmt_bits` 表查前特判返回 16（16bit
  容器，可配 bit_shift），照常过 pixfmt 校验与整除校验；装载前读首帧
  探测，把 pixfmt 原地替换为具体 `bayer_*16le`。
- `extract.rs` 新增 `detect_bayer_layout(read, w, h)`：读首帧前 128 行
  （`take + read_to_end`，整除对齐 2×2 块），统计主/反对角相邻同色均值差
  （G 沿对角强相关 → 差小的一对对角位置是 G）与四位置均值（组内非 G 两
  位置均值大者判 R，R>B 是统计规律）。置信门槛：对角差比 5% 余量、
  R/B 相对差 1%、近零均值保护（`d_large==0` 或 `(m00+m11)<2.0` 判
  不可分）——置信不足返回 None，调用侧兜底 `bayer_rggb16le` 并 warn 日志。
- 接入两条路径：快路径 `open_entry` 新开句柄探测（不动播放流）；慢路径
  7z/RAR 落盘后 `File::open(dest)` 同款探测——**慢路径仅缺 bit_shift
  位移修正（8.8 定案），探测不受影响**（探测读的是原始容器数据）。
- encode 引擎 validate 入口显式拒绝 bayer_auto16le（报错文案指引写明
  具体排列）——探测是一次性读首帧的播放链路能力，编码序列逐帧探测
  既慢也无必要。
- 测试 `bayer_layout_detection`：四种排列各合成 32×16 数据逐一验证 +
  纯灰数据 None + 过小帧 None。

**过程坑**：合成测试数据造错一次——GBRG 的 G 在主对角 `(0,0)/(1,1)`
（G B / R G 才是合法棋盘），先写成 G B / G R 违反棋盘律导致用例假失败；
写 Bayer 测试数据先画 2×2 棋盘再对照四种命名。

### 8.11 DEFAULT_TOML 注释风格重排：节头误删与防漂移测试盲区（2026-11-09）

**需求变更**：用户嫌「注释固定 tab 缩进」不顺眼（尤其 `[raw]` 节键/注释/
示例混排参差），改为全顶格 + 节前 `# ==== 中文说明 ====` 横线标题 + 键间
空行分隔。存量 config.toml 由 backfill 只补缺键不动注释，需删除重生成。

**踩坑**：重排时把真节头（`[cache]` 等 8 个）**替换**成了注释标题——
注释里的 `[cache]` 不是 TOML 节头，键全部落到顶层被 serde 静默忽略。
更糟的是防漂移测试仍通过：各 table 字段反序列化失败后恰好用
serde 默认值兜底，`from_tpl == Config::default()` 假成立——**逐项比较
对"整节键丢失"不敏感**。用户看文件时发现节头没了才暴露。

**修复**：横线标题只写中文说明（去掉节名），真节头独立成行紧跟其下；
`default_template_matches_config_default` 增加节头存在性断言
（8 个 `\n[节头]\n` 文本检查），节头缺失时先于逐项比较炸出。

**教训**：① 注释里写 `[section]` 字样时极易顺手把真节头吞掉——标题行
不含节名，节头永远独立成行；② 默认值兜底的相等断言对"键丢失"有盲区，
模板类测试要加结构存在性检查（纯文本断言即可）；③ serde 静默忽略未知
顶层键 + table 字段默认值兜底 = 结构错误零报错，解析成功 ≠ 解析正确。

### 8.12 编码链路接入文件名分辨率兜底：resolve_encode_format（2026-11-13）

**背景**：8.9 的文件名分辨率兜底只落在播放链路 `raw_demux_opts`，编码
链路 `encode_format` 直接透传预设宽高——预设填 0 时引擎报"宽高不可为 0"，
预设配错分辨率时只能编码失败，文件名里写好的 WxH 白带（用户实测发现）。

**修复**：新增 `resolve_encode_format(preset, entries, vfs)`（encode/cli.rs），
候选链与帧整除**直接引用 rawdec 媒体能力层原语**（`dims_candidates` /
`frame_divisible`，本次从 raw_demux_opts 内联逻辑下沉为共享函数）——预设
宽高非 0 在前 + 全体成员文件名候选在后（去重），逐候选对**全体成员**做帧
整除校验（播放是逐文件，编码是整序列共享一个 fmt，故全体过才算数），首个
全过者胜出。三个 collect 通道（文件/目录/压缩包）与 UI 确认窗组装统一改走
此函数，整除校验因此前置到序列收集期。bayer_auto16le 的解析见 8.13。

**连带**：`collect_archive` 测试原本用 8 字节假数据（收集期不校验整除），
新语义下被兜底逻辑拦下——改用真实帧大小（64x48 nv12 = 4608B）。整除
校验前置后，"总大小不是帧整数倍"类错误在确认窗组装期即报，不再等到
引擎启动（对 UI 更友好，CLI 报错时机不变仍为收集期）。

**教训**：两条链路（播放/编码）消费同一份 raw 预设时，参数解析语义
（候选链、校验口径）应当对齐而非各自实现——8.9 落地时只改了播放侧，
编码侧的缺口隔了四天才被用户实测撞出来；新增"链路级共享机制"时应同步
盘点全部消费者。

### 8.13 编码链路接入 auto bayer 探测 + rawdec 原语下沉（2026-11-13）

**背景**：8.10 把 bayer_auto16le 定为"仅播放链路探测、编码显式拒绝"——
播放能自动探排列，编码同一批文件却要手工在预设写明排列，体验割裂
（用户提出能否编码也支持）。

**修复**（两部分）：
- **探测接入**：`resolve_encode_format` 在收集期对 bayer_auto16le 读首
  条目（`vfs.open_entry`，需 FastSeek 支持）跑 `detect_bayer_layout`，
  探测成功把伪格式替换为具体排列（确认窗即显示真实排列）；探测置信不足
  时与播放预览**同款兜底 RGGB**，并向确认窗警告区显式提示（返回值由
  `Result<EncodeFormat>` 改为 `Result<(EncodeFormat, Vec<String>)>`，附加
  警告并入编号断档警告列表）。
- **原语下沉**：8.12 时候选链+整除是在 cli.rs 内重实现的（教训里自己
  点名的问题），本次从 `raw_demux_opts` 内联逻辑下沉为 rawdec 共享函数
  `dims_candidates(pw, ph, name)` / `frame_divisible(size, w, h, bits)`，
  播放与编码引用同一份实现；encode → rawdec 单向依赖（rawdec 只依赖
  config，不成环），架构设计 §15.3 复用清单已登记。

**方案反转（初版为"置信不足即报错拒编"）**：初版落地后用户实测
`cmodel_640x360 - 副本.raw` 编码报"置信不足"，质疑"播放能确定排列、
编码为何不能"。排查发现**播放根本没有确定排列**：PowerShell 复现检测
数学，d_main/d_anti 对角差值比 0.993（远超 95% 门限）→ 两级启发式都
判不可靠 → 播放侧同样返回 None，只是 `resolve_auto_bayer` 静默兜底
RGGB 继续播；而兜底与真探测打的是**同一条 INFO 日志**
（"bayer 排列探测: bayer_rggb16le"），掩盖了兜底事实。据此反转：
- **编码兜底对齐播放**（所见即所得）：预览 = 同一探测 + 同一 RGGB 兜底
  + 同一 bit_shift 透传，预览颜色即产物颜色；用户依预览判断排列是否
  正确，不对再在预设写明排列。拒编只是把同一个猜测换成一次打断，不
  提供新信息；
- **兜底必须显式**：`resolve_auto_bayer` 日志拆分——真探测 INFO、
  兜底 WARN 写明"置信不足按 RGGB 兜底"；编码侧兜底进确认窗警告区。

**测试**：`resolve_encode_format_auto_bayer_detection`——16x8 合成 RGGB
数据（主对角 R/B 大差、反对角 G-G 强相关）探测为 rggb16le；纯灰数据
断言兜底 rggb16le 且警告区出现"置信不足"。收集期整除校验连带把
`collect_dir` 测试的 8 字节假数据改成真实帧大小。

**教训**：① 两条链路"看似不一致"时先核实弱侧的真实行为——播放的
"能确定"其实是静默兜底伪装的，日志同文案掩盖了分支；新增探测类机制
时兜底路径必须有可区分的可观测性（日志级别/文案/UI 提示）；② 对齐
原则选"所见即所得"：预览能出的画面，编码应产出同款结果并把不确定
性显式暴露给用户，而不是单方面提高门槛拒编；③ 共享逻辑的下沉时机宜
早——先重实现再下沉等于把同一逻辑写两遍，第二次还要连带改测试。

### 8.14 探测算法重写：逐像素差分 → 位置均值统计（2026-11-13）

**现象**：用户反馈"非常清晰的 raw 也探不出来"。结合 8.13 的 cmodel 噪声
数据，旧算法在两类截然相反的数据上都误判不可靠。

**根因**：旧版 G 对角判别用**逐像素差分 L1 和**（|p00-p11| 逐块累加比
对角比值，95% 门限）。该统计把"跨通道差异"与"像素间涨落"（噪声 + 纹理
细节）混在一个量里：细纹理/高噪声下同通道相邻像素差分与跨通道差分
同量级 → 比值≈1 → 判 None。而旧版的均值统计（测试②）天然抗噪——数千
块取均值后噪声被平均掉、纹理不改变均值——却只用于 R/B 判别，没用于 G
对角判别。

**修复**：重写 `detect_bayer_layout`，两级判别全部改用位置均值：
- G 对角 = 对内均值差小的对角线；门限两个：结构性（较大对角差 > 总
  均值×0.5%，排纯灰/平场）+ 分离度（较小对角差 < 较大者×40%）；逐像素
  差分统计整体删除；
- R/B 判别保留均值约定（亮者判 R），删除 2.0 绝对门限，改全黑对守卫；
- **门限全部无量纲化**（用户提出"启发式需要准确 bit 数"——正确直觉的
  更优解：位深/对齐只缩放数值尺度，不改比值统计，门限做成比值后探测
  对 8~14bit 任何对齐天然不变，无需先猜位深；另有"从直方图估有效位深"
  的路子，但引入额外猜测环节，弃）。rawpy/libraw 并无像素级排列猜断
  （其靠 DNG/厂商元数据），裸 dump 无元数据，统计法是唯一路径。

**测试**：`rawdec::detect_bayer_layout_均值统计对噪声纹理稳健`——高噪声
（±900 均匀噪声淹没逐像素差分）、高纹理（跨通道均值差 100 vs 逐像素
涨落 ±1500，旧算法必判 None）、干净 GBRG、纯灰 → None、整体×16 位深
缩放不变量，五组直测。cmodel 噪声数据代入新算法：dm=57/da=7/level=756
→ 结构性 ✓ 分离度 ✓ → 由"兜底 RGGB"变为"真探出 rggb16le"。

**教训**：① 抗噪的关键是**先平均再比较**——逐像素统计把噪声带进比较，
块内均值统计把噪声留在门限之外；同一份统计数据（位置均值）其实足够
支撑两级判别，旧版却一半用差分一半用均值，纯属设计割裂；② "需要更
多先验（位深）"和"让统计量与先验无关（无量纲化）"都能回应同一缺陷，
后者少一个猜测环节、少一类失败模式。

### 8.15 需求变更：慢路径 bit_shift 补齐 + 新增 yuv420/rgb 通用预设（2026-11-18）

**需求**：① 7z/RAR 落盘播放 `.raw` 支持 bit_shift（废止 8.8 的"仅快路径"
定案）；② 新增两条通用默认预设：`*.yuv420`→yuv420p（planar I420）、
`*.rgb`→rgb24，软解（对比视图/直方图/探针）同步支持这两种格式。

**慢路径补齐（SingleFileVFS）**：重审 8.8 否决的三候选，发现 FileVFS
候选的否决前提（"须给 stream_cb 增设注册通道"）不成立——
`register_with_progress` 本就接受任意 ArchiveVFS。实现：

- 新增 `vfs/file_vfs.rs`：`SingleFileVFS` 把单个落盘文件包装成单条目
  FastSeek VFS（条目名 = 文件名，复用 `dir_vfs::FsFile` 适配器，size 构造
  时取一次 metadata）；
- `extract.rs` `load_extracted`：`shift > 0` 时注册 SingleFileVFS 进
  stream_cb 注册表 → `p.load(id, 0, &opts, shift)` 走 `zvf://` 流层
  ShiftReader（与快路径同源实现）；`shift = 0` 保持 `load_path` 零改动。
  注册失败（文件被删/占用）兜底走旧 `load_path` 行为；
- id = hash(落盘路径)：逐文件唯一、跨重启稳定；不同 shift → 不同 URI
  → mpv 缓存天然隔离（同 8.8 语义）。LRU 驱逐语义与 file:// 直读逐点
  一致（播放中句柄存活、切回重解后同路径覆盖注册项、open 失败走既有
  END_FILE(ERROR) 回显）；
- 缓存忠实拷贝语义零破坏：位移只在流层发生、不落盘、不进缓存键（当年
  否决后两候选的理由依然成立）。

**yuv420p / rgb24 软解**（`rawdec.rs`，探针集 = 软解集约束同 8.10）：

- `yuv420p_to_rgb`：与 nv12 的差别只在 UV 平面组织（两个独立 (W/2)×(H/2)
  平面 vs nv12 行内交错），BT.601 转换同款；
- rgb24 直通：逐像素 R,G,B 字节序与 egui `Color32::from_rgb` 同序，零转换；
- `RawProbe` 新增 `Rgb` 变体；yuv420p 探针复用 `Nv12` 变体（4:2:0 取样
  语义相同，V 平面偏移 = U 平面尾后）；panels.rs 探针显示同步。

**默认预设三条 → 五条**：`RawConfig::default()` / DEFAULT_TOML / README
同步；防漂移测试改 `raw_default_presets_builtin`（len 5 + 两新后缀可见性）。
老配置不受影响（serde 整体替换语义不变）。

**测试**：file_vfs 单测（list/open/seek/未知条目拒绝/文件缺失报错）；
`yuv420p_to_rgb_独立uv平面`（平面偏移/中性灰/数据不足）；探针测试扩
yuv420p 与 rgb24 用例；`raw_frame_info_帧数与软解判定` 改判 yuv420p 应
成功（8.8 时它是不在名单的负面用例）——用 yuv444p 接替负面角色。

**设计记录**：[ARCHITECTURE §13.6](../架构设计/13_渲染架构迁移.md) bit_shift
节已改为"快慢路径均生效"，8.8 定案段加追记指向本节。
