# Archview — 压缩包媒体播放器

直接预览播放压缩包内媒体的桌面工具：无需完整解压，打开 ZIP / 7Z / RAR 压缩包即可浏览和播放其中的视频与图片，支持加密压缩包（打开阶段自动探测密码 + 会话内记忆）。

## 功能概览

| 能力 | 说明 |
|---|---|
| 压缩格式 | ZIP（含 deflate/store）、7Z、RAR（含分卷 RAR） |
| 流式播放 | ZIP 直接边读边播（zvf:// 自定义流协议），不用先解压 |
| 后台解压 | 7Z / RAR 自动后台解压到缓存目录，完成后无缝播放 |
| 随机跳看 | 固态包跳转零打断：解码会话常驻，任何跳转只重定向目标、不重启解压；预取窗口随观看位置滑动，连续翻页跨批无等待断崖，看过的条目再点秒开 |
| 加密支持 | 打开阶段自动探测密码（无密码 → config 密码列表），耗尽才弹密码框；多编码候选兼容汉字密码；会话内密码记忆（切换包后切回不用重输） |
| 文件列表 | 视频 / 图片两个标签页，序号 `[当前/总数]` 显示，支持搜索过滤，多级子目录平铺展示 |
| 连续看包 | N 键直接弹出同目录压缩包列表；或翻到本包最后一项再往后翻（PgDn / 视频区滚轮 / ">" 按钮）时弹出。默认选中下一个，末包循环回第一个；分卷 RAR 只列首卷，点选任意包打开；本会话看过的包灰色显示；`ui.ask_next_archive` 可关。目录集合模式没有"下一个压缩包"概念，翻到末尾仅顶栏提示 |
| 播放控制 | mpv 风格快捷键、底部全宽进度条拖动 seek、音量调节、静音提示 |
| 图片自动播放 | PPT 式自动翻页（控制栏播放/暂停按钮或空格切换） |
| 全屏 | 回车进入，Esc 退出，全屏内快捷键照常可用（双击画面留给图片缩放，不再触发全屏） |
| 缓存管理 | 手动清理/打开目录按钮 + 可配置的退出/切换清理 + LRU 容量上限；缓存按包路径稳定命名，切回重开均可命中 |
| VarTidy 整理 | `archview.exe tidy` CLI 子命令：var 资源包分类后增量合入 var 仓库（零全量解压、包内/仓库两级去重、仓库只读绝不覆盖；详见下文「VarTidy」小节） |
| 配置集中 | 所有可变项集中在 config.toml，代码零硬编码路径 |

## 快捷键

**通用（视频 / 图片两模式）**

| 按键 | 功能 |
|---|---|
| 空格 | 视频 = 暂停 / 播放；图片 = 自动翻页（PPT 式播放）开关 |
| ↑ / ↓ | 音量 ±5（调整时自动解除静音） |
| M | 静音切换（静音时音量条旁显示红色"静音"标识，点击可解除） |
| PgUp / PgDn | 上一个 / 下一个媒体（同类型内导航） |
| N | 直接弹出「下一个压缩包」选择窗（连续看包；翻到本包末项再往后也会弹） |
| B | 回到锚点（最近一次跳看 / 解压前的出发条目，与列表底部悬浮按钮一致） |
| 回车 | 全屏切换 |
| Esc | 退出全屏 |

**视频模式**

| 按键 | 功能 |
|---|---|
| ← / → | 快退 / 快进 5 秒 |
| ← / →（暂停时） | 上一帧 / 下一帧（逐帧查看） |
| L | 播放模式切换：列表播放 ↔ 单视频循环（图片模式不响应） |
| 鼠标滚轮（视频区） | 上一个 / 下一个媒体（上滚 = 上一个；节流间隔 `ui.wheel_switch_secs`，防触控板惯性连跳） |

**图片模式**

| 按键 | 功能 |
|---|---|
| ← / → | 上一张 / 下一张（视频模式下才是 seek） |
| + / = 、 − | 放大 / 缩小（×1.25 步进，锚视口中心） |
| 0 | 100% 原始尺寸 |
| Home | 适应窗口 |
| 鼠标拖拽 | 平移已放大的图片 |
| 双击图片 | 适应窗口 ↔ 100% 切换（Honeyview 等看图软件习惯；全屏只走回车 / Esc，双击不再触发全屏） |
| 鼠标滚轮（图片区） | **一轮两用（按缩放状态分层）**：适应窗口态 = 翻上/下一张（与视频区同款节流）；缩放态 = 以光标为锚缩放（上滚放大），缩小触底自动落回适应窗口——继续滚即翻页，无需先退出缩放 |
| Ctrl + 滚轮（图片区） | 任意状态都以光标为锚缩放 |

滚轮按区域区分：媒体区（视频 / 图片）如上表，文件列表区的滚轮是普通滚动列表。搜索框等文本输入框聚焦时，快捷键自动失效，不会误触发。

## 使用方法

### 直接运行（推荐用户）

1. 解压发布包（archview.exe + libmpv-2.dll + config.toml），双击 `archview.exe`。
2. 点击左上角 **Open Archive…** 选择压缩包，或直接把压缩包文件拖入窗口。
3. 左侧标签页切换 **视频 / 图片**，顶部搜索框可过滤文件名；点击文件即开始播放，悬停行的完整文件名显示在列表底部状态行。
4. 右侧播放区底部为进度条，拖动到目标位置松手即跳转。

> 首次启动若 exe 同目录没有 config.toml，会自动生成一份默认配置。

### VarTidy：var 资源包增量整理（CLI 子命令）

`archview.exe` 无参数启动 = 播放器 GUI（行为不变）；带 `tidy` 参数 = var 资源包整理。var 文件不可播放，整理与播放器 UI 无交集，因此走 CLI 而非塞进 GUI——替代手动流程「解压 → class_var.py → deal 脚本 → 删空目录 → 手动并入仓库 → 去重」，实现零全量临时落盘的原地整理（50G 包手动流程需额外 ~100G 磁盘，新流程磁盘增量 ≈ 仓库确实缺少的 var 体积）。

```bat
archview.exe tidy <压缩包或目录> [更多输入...] [选项]
```

| 选项 | 说明 |
|---|---|
| `-o, --repo <dir>` | var 仓库目录（缺省用 config.toml 的 `vartidy.repo_dir`） |
| `-n, --dry-run` | 预演：只做分类与两级去重检查并输出报告，不写任何文件 |
| `-p, --password <pw>` | 加密包密码（可多次指定按序尝试；config.toml 顶层 `passwords` 列表兜底） |
| `-q, --quiet` | 静默：无控制台时结束不弹结果窗（脚本/计划任务防阻塞） |
| `--report <dir>` | 报告输出目录（缺省为首个输入所在目录，不写入仓库） |

行为要点：

- **仓库只读红线**：仓库已有文件/文件夹绝不被修改、移动、删除、覆盖；仅创建新作者子目录并写入仓库没有的新 var；同名冲突一律仓库版胜出。
- **两级去重**：包内（多级目录同名 var 按 CRC32 元数据判重，同名不同内容记警告交人工确认）+ 仓库（目标存在即跳过）。
- **无效文件不落盘**：非 var / 命名非法（版本号非数字、作者/内容缺失等）只进报告，仓库零垃圾。
- 支持中文、空格、特殊字符的作者名与压缩包名；加密包（含中文密码）；分卷 RAR（同族只整理首卷）。
- 退出码：0 = 完成；1 = 有失败项（见报告）；2 = 用法/配置错误。

**「发送到」快捷方式**：`sendto_setup.bat` 一键创建两个菜单项——放在 archview.exe 同目录（package.bat 打包后即如此）双击即可，exe 路径自动检测；也可用 bat 顶部的 `EXE_PATH` 覆盖：

- **Archview player**：右键压缩包（可多选）→ 发送 → 直接用播放器打开（多选时打开第一个，同目录其余包按 N 键切换）。也适用于把压缩包/目录拖到 exe 上、文件关联等任意「带路径启动」场景。
- **Archview tidy**：右键压缩包（可多选）→ 发送 → 按 config.toml 的 `vartidy.repo_dir` 增量整理。

已存在则覆盖重建（换 exe 位置重跑一次即可）。手动创建也可以：在 `shell:sendto` 文件夹（`%APPDATA%\Microsoft\Windows\SendTo`）新建快捷方式，目标分别填 `"<archview.exe 完整路径>"` 和 `"<archview.exe 完整路径>" tidy`——**不要带 `"%1"`**（那是注册表 shell 命令的语法，快捷方式里会当字面量传入；选中文件由 Windows 自动追加为参数）。

架构与设计原则详见 `ARCHITECTURE.md` 第 12 章。

### 从源码构建

环境要求：Rust（MSVC toolchain）。

```bat
git clone <本仓库地址>
cd archview
cargo build --release
```

### 获取 libmpv-2.dll

Archview 的视频播放基于 libmpv，**仓库不含该 dll**（约 120MB，超出 GitHub 单文件限制）。获取方式任选其一：

1. **推荐**：下载 [shinchiro 的 mpv 构建产物](https://github.com/shinchiro/mpv-winbuild-cmake/releases)（`mpv-x86_64-*.7z`），解压后取其中的 `libmpv-2.dll`
2. 或使用 [mpv 官方构建](https://mpv.io/installation/) 中附带的 dll

放置位置：`deps\mpv-dev\libmpv-2.dll`（开发期由 build.rs 自动复制到 target 目录），或直接与 exe 同目录。

### 打 release 包

```bat
package.bat            rem 完整包（含 libmpv-2.dll，约 120 MB）
package.bat no-mpv     rem 精简包（不含 mpv，自行下载放 exe 同目录，见下方依赖说明）
```

产物：`dist\`（exe + libmpv-2.dll + config.toml）及 `dist.zip`。可用环境变量覆盖路径（不硬编码）：

- `ARCHVIEW_MPV_DIR`：libmpv-2.dll 所在目录（默认 `deps\mpv-dev`）
- `ARCHVIEW_DIST`：输出目录（默认 `dist`）

## 配置文件 config.toml

启动时按「exe 同目录 → 当前工作目录」顺序查找；不存在则生成一份**带中文注释**的默认配置；已有配置缺失新版本新增的键时，启动时自动按默认值补全并写回（已有键值与注释原样保留）。所有路径均可为绝对路径，`"auto"` 表示自动探测。

完整默认模板（各项含义见下表；生成的 config.toml 内含逐键中文注释）：

```toml
# 非 UTF-8 字节兜底代码页：老工具按系统 ANSI 代码页打包，密码与 zip 文件名同源编码。
# 用于密码候选追加 + zip 文件名解码兜底（GBK 名老包不再乱码/无法播放）
codepage = "gbk"

# 加密包密码候选列表（播放器与 VarTidy 公用，按序尝试）。
# 打开压缩包阶段即自动探测；密码始终用双引号包裹
passwords = []

# 主程序日志级别：error / warn / info / debug / trace
log_level = "info"

[cache]
# libmpv-2.dll 搜索目录；"auto" = exe 同目录
dll_dir = "auto"
# 7Z/RAR 解压缓存目录；"auto" = %TEMP%\archview_cache（可指向 RAM Disk 提速）
dir = "auto"
# 缓存总量上限（MB），超出按 LRU（最久未访问）驱逐
max_size_mb = 8192
# 解码块大小（MB）与解码块 LRU 缓存块数（ZIP 流式播放的内存占用）
chunk_size_mb = 4
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
# mpv 缓存秒数与 demuxer 最大缓存（MB）
cache_secs = 30
demuxer_max_mb = 512
# demux 探测字节/时长预算：压缩包内流逐字节解压，预算直接换算成打开耗时
demuxer_probesize = "2MiB"
demuxer_analyzeduration = "1"
# 初始音量（0-100）
volume = 100
# mpv 内部日志（诊断打开慢/解码链路时临时设 "v"，平时 "no"）
mpv_log_level = "no"

[ui]
# CJK 字体路径；"auto" = 自动加载系统中/日/韩候选字体链（支持中日韩文件名显示）
font_path = "auto"
# 图片自动播放（PPT 式翻页）间隔秒数
slideshow_secs = 1.0
# 媒体区滚轮切换的最小间隔秒数（防惯性/触控板连跳）
wheel_switch_secs = 0.3
# 翻到本包最后一项再往后导航时，是否弹窗选择同目录其它压缩包
ask_next_archive = true
# 文件列表是否按自然序排序（false = 保持压缩包内部顺序，漫画包场景开 true）
sort_files = false
# 解压进度覆盖层显示前的宽限期（毫秒）：快速解压全程不打扰播放
extract_overlay_delay_ms = 500
# 图片 egui 纹理直显（后台解码直接上纹理，翻页秒开；解码失败自动回退 mpv）
image_texture = true
# 图片纹理 LRU 缓存张数（含当前张）：预解码相邻页，翻页零等待
image_cache_count = 3
# 基础纹理档尺寸上限；高清档升级资格判据同此值
image_max_dim = 4096
# 高清档触发阈值：zoom×ppp 超过此值才解码 full 级纹理
image_full_threshold = 2.0
# 高清档放大后叠加 unsharp mask 锐化
image_full_sharpen = true

[extract]
# solid 包解压时顺带预取后续文件的总大小上限（MB，下游预取），0 = 关闭
prefetch_downstream_mb = 512
# 上游预取上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置条目
# （7z 限于目标所在 folder 内、RAR 为整个前缀）顺带写盘，随机跳看后
# 往回翻直接命中缓存。解码零增量，仅占磁盘写入量
# （旧键名 prefetch_mb / prefetch_prev_mb 仍可读取）
prefetch_upstream_mb = 512

[vartidy]
# var 仓库根目录（archview.exe tidy 用；整理器绝不修改仓库已有内容，仅增量合入）
repo_dir = ""
# 报告输出目录；"auto" = 首个输入压缩包所在目录
report_dir = "auto"
```

| 配置项 | 说明 |
|---|---|
| `codepage` | 非 UTF-8 字节兜底代码页（老工具按系统 ANSI 编码密码与 zip 文件名），如 `gbk` / `big5` / `shift_jis` / `euc-kr`；`none` 只用 UTF-8。密码侧追加候选；文件名侧解码无 UTF-8 标志且非合法 UTF-8 的 zip 条目名（GBK 名老包） |
| `passwords` | 加密包密码候选列表（播放器与 VarTidy 公用，按序尝试）：打开阶段自动探测（无密码优先 → 列表依次），全不匹配播放器才弹密码框、CLI 才记失败。密码必须双引号包裹，仅用于解压，不写入日志/报告 |
| `log_level` | 主程序日志级别 `error` / `warn` / `info` / `debug` / `trace`；支持 EnvFilter 按模块指令只开某模块的诊断日志（如 `"info,archview::ui::panels=debug"`）；RUST_LOG 环境变量优先 |
| `cache.dll_dir` | libmpv-2.dll 搜索目录，`auto` = exe 同目录 |
| `cache.dir` | 7Z/RAR 解压缓存目录，`auto` = `%TEMP%\archview_cache`；可指向 RAM Disk 提速（见下文"进阶：RAM Disk 缓存"）；目录不存在会自动创建 |
| `cache.max_size_mb` | 缓存总量上限（MB），超出按 LRU（最久未访问）驱逐 |
| `cache.chunk_size_mb` | ZIP 流式播放的解码块大小（MB） |
| `cache.chunk_cache_blocks` | 解码块 LRU 缓存块数；ZIP 流式播放的内存占用 ≈ 两者乘积（默认 4 × 64 = 256MB） |
| `cache.clean_on_exit` | 退出程序时自动清空解压缓存（默认 false） |
| `cache.clean_on_switch` | 切换压缩包时是否清理上一个包的缓存（默认 false，保留以便切回时命中） |
| `media.*_extensions` | 视频 / 图片的文件扩展名识别列表（不区分大小写） |
| `player.hwdec` | 硬件解码（`auto` / `no` / `d3d11va` 等 mpv 取值） |
| `player.cache_secs` | mpv 缓存秒数，拖回刚看过的片段直接命中缓存，零成本 |
| `player.demuxer_max_mb` | mpv demuxer 最大缓存（MB） |
| `player.demuxer_probesize` | demux 探测字节预算（打开阶段识别容器格式的读取上限）。压缩包内流 = 逐字节解压，预算直接换算成打开耗时，调小即提速；`auto` = mpv 原生默认 ≈5MB；过小可能识别不了冷门容器 |
| `player.demuxer_analyzeduration` | demux 探测时长预算（秒，确定各流参数的分析上限）。`auto` = mpv 原生默认 ≈5s；过小可能漏识别延迟出现的音轨/字幕流 |
| `player.volume` | 初始音量（0-100），播放中 ↑/↓ 调整（步进 5） |
| `player.mpv_log_level` | mpv 内部日志级别（`no` / `fatal` / `error` / `warn` / `info` / `status` / `v` / `debug` / `trace`）。诊断 mpv 内部行为（打开慢 / 解码链路）时临时设 `v`——量极大，转发到主程序日志；平时保持 `no` |
| `ui.font_path` | CJK 字体路径，`auto` 加载系统中/日/韩候选字体链（msyh / simhei / simsun + YuGoth / Meiryo / MS Gothic + Malgun，同族按序补字形，支持中日韩文件名显示）；也可指定单个字体文件的绝对路径 |
| `ui.slideshow_secs` | 图片自动播放（PPT 式翻页）间隔秒数 |
| `ui.wheel_switch_secs` | 媒体区滚轮切换的最小间隔秒数（防惯性 / 触控板连跳）：视频区滚轮换媒体与图片适应窗口态滚轮翻页共用此节流 |
| `ui.ask_next_archive` | 翻到本包最后一项再往后导航时，是否弹窗选择打开同目录其它压缩包（默认 true；列表默认选中循环意义上的下一个，末包绕回第一个；目录内没有其它包时仅顶栏提示） |
| `ui.sort_files` | 文件列表是否按自然序排序（数字段按数值比较、不区分大小写，默认 false）。false = 保持压缩包内部顺序——7z/RAR 固态包下内部顺序即物理码流顺序，预取直接命中相邻文件（性能最优）；注意 7-Zip 工具打的包内部本来就是字典序，关排序对这类包显示无变化。true = 自然序（`1/2/10`，而非字典序 `1/10/2`），看漫画/图集时开启 |
| `ui.extract_overlay_delay_ms` | 解压进度覆盖层的宽限期（毫秒）：播放中切到需解压的条目，超过该时长仍未解完才隐藏视频显示进度；快速解压全程不打扰播放 |
| `ui.image_texture` | 图片走 egui 纹理直显（后台解码直接上纹理，翻页秒开；静图与动图 gif 均走此链路）。解码失败的图片自动回退 mpv |
| `ui.image_cache_count` | 图片纹理 LRU 缓存张数（含当前张），预解码 ±1 相邻页，翻页零等待 |
| `ui.image_max_dim` | 基础纹理档尺寸上限（预放大 ×ppp 后超过则原尺寸直进）；full 高清档升级资格判据同此值（详见 ARCHITECTURE.md 13.4 两档策略） |
| `ui.image_full_threshold` | 高清档触发/换画阈值：屏幕物理像素超过基础纹理该倍数（zoom×ppp > 此值）才解码/换画 full 级纹理。默认 2.0 = 越过 244% 滚轮档（×1.25⁴ ≈ 2.44）才升级；调小（如 1.4）更早升级对比，调大（如 3.0）基本关闭 |
| `ui.image_full_sharpen` | 高清档 Lanczos 放大后叠加 unsharp mask 锐化（补偿插值摊平的边缘坡度；强对比边缘可能有轻微光晕，可关闭对比） |
| `extract.prefetch_downstream_mb` | 下游预取上限（MB），0 = 关闭：solid 包（7z / solid RAR）解压时顺带预取下标更大的后续文件，一次顺序解压流解出一批，翻页直接命中 |
| `extract.prefetch_upstream_mb` | 上游预取上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置条目顺带写盘，随机跳看后回看直接命中缓存（旧键名 `prefetch_mb` / `prefetch_prev_mb` 仍可读取） |
| `vartidy.repo_dir` | VarTidy（`archview.exe tidy` CLI 子命令）的 var 仓库目录，必须已存在（整理器绝不修改仓库已有内容）；空 = 未配置，CLI 需 `-o` 指定；详见 ARCHITECTURE.md 第 12 章 |
| `vartidy.report_dir` | 报告输出目录，`auto` = 首个输入压缩包所在目录（不写入仓库，保持仓库纯净） |

### 路径写法（cache.dir 等所有路径项）

TOML 字符串，双引号包裹。Windows 下正斜杠、反斜杠都认，但反斜杠必须**双写**（TOML 转义）：

```toml
dir = "D:/my_cache"       # 推荐：正斜杠，无需转义
dir = "D:\\my_cache"      # 反斜杠必须双写，单个 \ 会解析报错
dir = "auto"              # 特殊值：自动探测
```

常见错误：`#` 前有空格会被当注释起点（值里含 `#`、空格等特殊字符——尤其密码——必须双引号包裹，引号内一切字符原样）；`dir = D:/cache` 缺引号不是合法 TOML。相对路径相对的是 exe 启动时的工作目录，建议写绝对路径。目录不存在会自动创建。

### 进阶：RAM Disk 缓存（可选推荐）

7Z/RAR 走"后台解压到磁盘缓存再播放"的慢路径，缓存落在机械盘上时首次播放等待明显。把 `cache.dir` 指向 RAM Disk 可以显著加速，且**断电/重启即自动清空**，无需维护：

1. 用 [ImDisk Toolkit](http://www.ltr-data.se/opencode.htm/)、OSFMount 或 [Primo Ramdisk](https://www.romexsoft.com/zh/primo-ramdisk/) 等工具划出一块 RAM Disk（建议 2~4GB，盘符如 `R:`）
2. 修改 config.toml：

```toml
[cache]
dir = "R:/archview_cache"
max_size_mb = 3072
clean_on_exit = false
clean_on_switch = false
```

3. `max_size_mb` 建议设为 RAM Disk 容量的 70%~80%，留出余量；超出上限时按 LRU 自动驱逐最久未访问的缓存

> RAM Disk 容量有限：单条目超过剩余空间时（如蓝光原盘），解压前会预检磁盘余量并**直接报错提示**（不再发起注定失败的解压）——此时需手动把 `cache.dir` 改回磁盘目录并重启程序。`clean_on_exit` / `clean_on_switch` 保持 false 即可：RAM Disk 生命周期与系统一致，重启自动归零。

### 缓存清理时机

1. **手动**：顶栏「清理缓存」按钮，显示释放空间；「缓存目录」按钮用资源管理器打开。
2. **切换压缩包**：`clean_on_switch` 为 true 时删除上一个包的解压缓存（按 archive_id 前缀匹配）。
3. **退出程序**：`clean_on_exit` 为 true 时清空整个缓存目录。
4. **容量上限**：超过 `max_size_mb` 时 LRU 驱逐最旧的文件。
5. **启动**：清理上次中断解压留下的 `.part*` 半截文件。

缓存按压缩包路径稳定命名：同一包 A→B→A 切回（或程序重启后重开）缓存直接命中，无需重新解压。

## 已知限制与规划

- 分卷 ZIP：暂不支持（分卷 RAR 已支持）。
- 兜底代码页只用一个配置编码（默认 GBK/简中）：密码侧只追加一个候选；zip 文件名侧 UTF-8 校验失败后也只用它解码。包若在日文等其它代码页系统上创建，需把 `codepage` 改为对应编码（如 `shift_jis`）。GBK/Shift-JIS 等双字节编码之间无法从字节层自动区分。
- 视频缩略图、播放进度记忆：规划中。

## 更多文档

- [ARCHITECTURE.md](ARCHITECTURE.md)：架构设计——VFS 抽象层、双路径播放、密码链路、缓存策略、线程模型、模块清单、大包性能设计。
- [DEVLOG.md](DEVLOG.md)：修复记录 / 踩坑手册 / 日志排查对照 / 测试运行方式与覆盖矩阵。

## 开源协议

[MIT](LICENSE)。注意：本程序运行时动态加载的 libmpv 以其自身的开源协议（GPL/LGPL）分发，dll 需自行获取（见上文"获取 libmpv-2.dll"）。
