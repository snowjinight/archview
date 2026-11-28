# Archview — 压缩包 / 光盘镜像媒体播放器

把压缩包当文件夹用、不解压直接看片看图的桌面工具：ZIP / 7Z / RAR（含分卷、加密包）拖进窗口，里面的视频和图片立刻成列、点击即播；ISO / UDF 光盘镜像同样直接打开——蓝光原盘（BDMV/M2TS）、DVD（VOB）不用挂载、不用虚拟光驱，像打开一个普通文件夹一样浏览播放。
不止压缩包和镜像：普通文件夹拖入即用；固态压缩包随机跳看秒开（后台解码会话常驻，翻到哪解到哪）；相机 RAW、无封装的裸数据流（yuv / nv12 dump）也能直接播放；多图分栏对比、raw 序列帧一键编码成视频（encode）、var 资源包增量整理（tidy）——从素材到成片，一个工具全包。

文档按「通用 → 播放器 → Encode → VarTidy」组织：播放器是主功能（标准媒体 + 非标裸数据播放），Encode 与 VarTidy 是复用同一内核的两个子功能（GUI 右键 + CLI 子命令）；各项配置不集中罗列，分散到对应功能的章节里随功能介绍。

## 功能概览

| 能力 | 说明 |
|---|---|
| 压缩格式 | ZIP（含 deflate/store）、7Z、RAR（含分卷 RAR）；光盘镜像 ISO（ISO9660 + Joliet + 纯 UDF 2.50，蓝光 BDMV/M2TS 与 DVD-VOB 原盘直读） |
| 加密支持 | 打开阶段自动探测密码（无密码 → config 密码列表），耗尽才弹密码框；多编码候选兼容汉字密码；会话内密码记忆（切换包后切回不用重输） |
| 文件列表 | 视频 / 图片两个标签页，序号 `[当前/总数]` 显示，高级搜索过滤（见「播放器 → 高级搜索」），多级子目录平铺展示；单击 = 纯播放（与选区解耦），多选 = Ctrl / Shift / Ctrl+A（见「播放器 → 多选操作」） |
| 流式播放 | ZIP 直接边读边播（zvf:// 自定义流协议），不用先解压 |
| 后台解压 | 7Z / RAR 自动后台解压到缓存目录，完成后无缝播放 |
| 随机跳看 | 固态包跳转零打断：解码会话常驻，任何跳转只重定向目标、不重启解压；预取窗口随观看位置滑动，连续翻页跨批无等待断崖，看过的条目再点秒开 |
| 连续看包 | N 键直接弹出同目录压缩包列表；或翻到本包最后一项再往后翻（PgDn / 视频区滚轮 / ">" 按钮）时弹出。默认选中下一个，末包循环回第一个；分卷 RAR 只列首卷，点选任意包打开；本会话看过的包灰色显示；`ui.ask_next_archive` 可关。目录集合模式没有"下一个压缩包"概念，翻到末尾仅顶栏提示 |
| 裸数据播放 | 非标裸数据（`.yuv` / `.bin` dump 等）经 `[[raw.demux]]` 预设直接播放：内置 nv12 / yuv400 / yuv420 / rgb / raw 五条默认预设零配置可用，宽高可从文件名内嵌 `WxH` 兜底，Bayer 排列可自动探测；详见「播放器 → 非标裸数据播放」 |
| 播放控制 | mpv 风格快捷键、底部全宽进度条拖动 seek、音量调节、静音提示 |
| 图片自动播放 | PPT 式自动翻页（控制栏播放/暂停按钮或空格切换） |
| 全屏 | 回车进入，Esc 退出，全屏内快捷键照常可用（双击画面留给图片缩放，不再触发全屏） |
| 缓存管理 | 手动清理/打开目录按钮 + 可配置的退出/切换清理 + LRU 容量上限；缓存按包路径稳定命名，切回重开均可命中 |
| 序列编码 | raw dump 序列帧后台编码为视频文件，不占用播放：UI 列表右键「编码为视频…」或 `archview.exe encode`（默认 H.264）；详见「Encode」 |
| VarTidy 整理 | `archview.exe tidy` CLI 子命令：var 资源包分类后增量合入 var 仓库（零全量解压、包内/仓库两级去重、仓库只读绝不覆盖）；详见「VarTidy」 |
| 配置集中 | 所有可变项集中在 config.toml，代码零硬编码路径；各项配置分散到对应功能章节随功能介绍 |

## 通用

### 快速上手（直接运行）

1. 解压发布包（archview.exe + libmpv-2.dll + config.toml + install.bat + uninstall.bat），双击 `archview.exe`。
2. 点击左上角 **Open Archive…** 选择压缩包，或直接把压缩包文件拖入窗口。
3. 左侧标签页切换 **视频 / 图片**，顶部搜索框可过滤文件名；点击文件即开始播放，悬停行的完整文件名显示在列表底部状态行。
4. 右侧播放区底部为进度条，拖动到目标位置松手即跳转。

5. （可选）双击同目录 `install.bat`，注册「发送到」菜单与压缩包/图片/视频的文件关联（详见下节）；卸载用 `uninstall.bat`。

> 首次启动若 exe 同目录没有 config.toml，会自动生成一份默认配置。

### 「发送到」与文件关联

`install.bat` 一次完成两项用户级集成（无需管理员权限，放在 archview.exe 同目录——package.bat 打包后即如此——双击即可，exe 路径自动检测；也可用 bat 顶部的 `EXE_PATH` 覆盖；卸载用同目录 `uninstall.bat`）：

**1. 「发送到」菜单**，一键创建三个菜单项：

- **Archview player**：右键压缩包（可多选）→ 发送 → 直接用播放器打开（多选时打开第一个，同目录其余包按 N 键切换）。也适用于把压缩包/目录拖到 exe 上、文件关联等任意「带路径启动」场景。
- **Archview tidy**：右键压缩包（可多选）→ 发送 → 按 config.toml 的 `vartidy.repo_dir` 增量整理（见下文「VarTidy」）。
- **Archview encode**：右键帧文件/目录/压缩包（可多选）→ 发送 → 按 CLI encode 语义编码为视频（见下文「Encode」；多选帧文件即精确模式）。

**2. 文件关联**：按「压缩包 / 光盘镜像 / 视频 / 图片」注册四个 ProgID（`Archview.Archive` / `Archview.Iso` / `Archview.Video` / `Archview.Image`，`shell\open\command` 指向 exe `"%1"`；每个 ProgID 各自的 DefaultIcon 指向 exe 内嵌的对应类型图标），再把全部受支持扩展名以 `OpenWithProgids` 写入对应键——**只进右键「打开方式」推荐列表，不抢系统默认**。覆盖范围与 config 内置清单一致：

- 压缩包：`zip` `7z` `rar`；
- 光盘镜像：`iso`（ISO9660 / Joliet / 纯 UDF 2.50，蓝光 BDMV 与 DVD 原盘直读）；
- 视频：`mp4` `mkv` `avi` `mov` `flv` `ts` `webm` `wmv` `m4v`，加非标 raw dump 三后缀 `nv12` `yuv400` `raw`（`[[raw.demux]]` 默认预设；raw 走 `is_video_ext` 的 raw 预设命中，双击即打开父目录媒体集合并跳转该文件）；
- 图片：`jpg` `jpeg` `png` `gif` `webp` `bmp` `tif` `tiff`，加相机 RAW `arw` `nef` `nrw` `srf` `sr2` `arq`。

要把某类型设为默认打开方式，右键文件 → 打开方式 → 选择其他应用 → 始终（`UserChoice` 由 Windows 管理，脚本不代写）。exe 移动位置后重跑 install.bat 刷新路径；卸载跑 `uninstall.bat`（删 ProgID、逐扩展名摘除 OpenWithProgids 值、删 SendTo 快捷方式；已设默认的类型需在 设置 → 应用 → 默认应用 手动清除）。

已存在则覆盖重建（换 exe 位置重跑一次即可）。手动创建也可以：在 `shell:sendto` 文件夹（`%APPDATA%\Microsoft\Windows\SendTo`）新建快捷方式，目标分别填 `"<archview.exe 完整路径>"`、`"<archview.exe 完整路径>" tidy`、`"<archview.exe 完整路径>" encode`——**不要带 `"%1"`**（那是注册表 shell 命令的语法，快捷方式里会当字面量传入；选中文件由 Windows 自动追加为参数）。

### 配置文件 config.toml

启动时按「exe 同目录 → 当前工作目录」顺序查找；不存在则生成一份**带中文注释**的默认配置；已有配置缺失新版本新增的键时，启动时自动按默认值补全并写回（已有键值与注释原样保留）。所有路径均可为绝对路径，`"auto"` 表示自动探测。顶栏「配置 → 重载配置」（或内嵌配置编辑器保存）热生效，不重启：若当前正在播放/显示，当前项自动按新配置重载（图片重解码、视频销毁播放器重建后重新装载；per-file 亮度记忆保留）。

配置项按功能拆分、随对应章节介绍，本节只列全局项：播放器相关（`[cache]` / `[media]` / `[raw]` / `[player]` / `[ui]` / `[image]` / `[extract]`）见「播放器」节；`[encode]` 见「Encode」节；`[vartidy]` 见「VarTidy」节。

全局配置项（播放器与两个子功能公用）：

```toml
# 非 UTF-8 字节兜底代码页：老工具按系统 ANSI 代码页打包，密码与 zip 文件名同源编码。
# 用于密码候选追加 + zip 文件名解码兜底（GBK 名老包不再乱码/无法播放）
codepage = "gbk"

# 加密包密码候选列表（播放器与 VarTidy 公用，按序尝试）。
# 打开压缩包阶段即自动探测；密码始终用双引号包裹
passwords = []

# 主程序日志级别：error / warn / info / debug / trace
log_level = "info"
```

| 配置项 | 说明 |
|---|---|
| `codepage` | 非 UTF-8 字节兜底代码页（老工具按系统 ANSI 编码密码与 zip 文件名），如 `gbk` / `big5` / `shift_jis` / `euc-kr`；`none` 只用 UTF-8。密码侧追加候选；文件名侧解码无 UTF-8 标志且非合法 UTF-8 的 zip 条目名（GBK 名老包） |
| `passwords` | 加密包密码候选列表（播放器与 VarTidy 公用，按序尝试）：打开阶段自动探测（无密码优先 → 列表依次），全不匹配播放器才弹密码框、CLI 才记失败。密码必须双引号包裹，仅用于解压，不写入日志/报告 |
| `log_level` | 主程序日志级别 `error` / `warn` / `info` / `debug` / `trace`；支持 EnvFilter 按模块指令只开某模块的诊断日志（如 `"info,archview::ui::panels=debug"`）；RUST_LOG 环境变量优先 |

#### 路径写法（cache.dir 等所有路径项）

TOML 字符串，双引号包裹。Windows 下正斜杠、反斜杠都认，但反斜杠必须**双写**（TOML 转义）：

```toml
dir = "D:/my_cache"       # 推荐：正斜杠，无需转义
dir = "D:\\my_cache"      # 反斜杠必须双写，单个 \ 会解析报错
dir = "auto"              # 特殊值：自动探测
```

常见错误：`#` 前有空格会被当注释起点（值里含 `#`、空格等特殊字符——尤其密码——必须双引号包裹，引号内一切字符原样）；`dir = D:/cache` 缺引号不是合法 TOML。相对路径相对的是 exe 启动时的工作目录，建议写绝对路径。目录不存在会自动创建。

## 播放器（标准媒体 + 非标裸数据）

`archview.exe` 无参数启动即进入播放器 GUI。打开压缩包或目录后，标准媒体（mp4 等，mpv 直接装载）与非标裸数据（`[[raw.demux]]` 预设命中的 raw dump）走同一份列表与播放体验。

### 基本操作与快捷键

**通用（视频 / 图片两模式）**

| 按键 | 功能 |
|---|---|
| 空格 | 视频 = 暂停 / 播放；图片 = 自动翻页（PPT 式播放）开关 |
| PgUp / PgDn | 上一个 / 下一个媒体（同类型内导航） |
| 鼠标滚轮（预览窗口内） | 上一个 / 下一个媒体（上滚 = 上一个；节流间隔 `ui.wheel_switch_secs`，防触控板惯性连跳；图片缩放态滚轮 = 缩放，见图片模式） |
| N | 直接弹出「下一个压缩包」选择窗（连续看包；翻到本包末项再往后也会弹） |
| 回车 | 全屏切换 |
| Esc | 退出全屏 |
| F12 | 运行日志面板开关（诊断用：展示最近运行日志；任何焦点 / 弹窗下都可用，再按关闭） |
| Ctrl + A | 全选文件列表当前标签页的全部条目（多选；搜索框聚焦或密码框 / 配置编辑器 / 编码确认窗在场时不触发，避免误吞输入框的文本全选） |
| C | 对比显示：将选中集（Ctrl/Shift 多选，合格 2~6 个）进入分栏对比视图；也可用列表右键菜单「对比显示（C）」。详见下文「对比模式」 |
| X | 快速对比（**无选中时**）：以当前播放图片（或 raw dump）为起点，自动携带视图序下一个合格条目，2 栏进入对比（末项回绕到第一个；有选中时 X 不响应，选中集入口是 C）。详见下文「对比模式」 |
| [ / ] | per-file 显示亮度 −5 / +5（mpv gamma 标尺 −100~100，0 = 复位；视频 / 图片 / raw dump 通用，会话内按文件记忆，切回自动恢复；对比模式下对全部栏同调） |

B 键 = 跳至解压处（列表底部悬浮「跳至解压处」按钮同款）：跳回最近一次后台解压的条目（已落地秒开，未落地跳回等待、落地自动播放）。对任何需要解压才能看的包有效——7z / RAR 不分固态与否，点击条目都要先解压落地，每次真正解压（缓存未命中）都会推进锚点；流式 zip 直接流式播放不解压，无锚点，B 无处可跳。

**视频模式**

| 按键 | 功能 |
|---|---|
| ← / → | 快退 / 快进 5 秒 |
| ← / →（暂停时） | 上一帧 / 下一帧（逐帧查看） |
| ↑ / ↓ | 音量 ±5（调整时自动解除静音） |
| M | 静音切换（静音时音量条旁显示红色"静音"标识，点击可解除） |
| L | 播放模式切换：定格（默认，播完停末帧）→ 列表播放 → 单视频循环（图片模式不响应） |

**图片模式**

| 按键 | 功能 |
|---|---|
| 空格 | 自动翻页（PPT 式播放）开关 |
| ← / → | 上一张 / 下一张（视频模式下才是 seek） |
| + / = 、 − | 放大 / 缩小（×1.25 步进，锚视口中心） |
| 0 | 100% 原始尺寸 |
| Home | 适应窗口 |
| 鼠标拖拽 | 平移已放大的图片 |
| 双击图片 | 适应窗口 ↔ 100% 切换（Honeyview 等看图软件习惯；全屏只走回车 / Esc，双击不再触发全屏） |
| 鼠标滚轮（图片区） | **一轮两用（按缩放状态分层）**：适应窗口态 = 翻上/下一张（与视频同款节流）；缩放态 = 以光标为锚缩放（上滚放大），缩小触底自动落回适应窗口——继续滚即翻页，无需先退出缩放 |
| Ctrl + 滚轮（图片区） | 任意状态都以光标为锚缩放 |
| R | 顺时针旋转 90°（EXIF Orientation 轮进写回；写不回的场合会话内记忆） |
| 1 ~ 5 | 图片评级（星级；写 sidecar `.xmp`，压缩包内条目仅会话内记忆） |
| Q | 清除评级 |

**对比模式**（C 键 / 右键菜单「对比显示」进入；无选中时可按 X 快速进入——以当前播放图片为起点 + 下一个合格条目 2 栏对比；标准视频与动图 gif/webp 不参与；退出 = Esc 或退出对比后自动回放主栏）

| 按键 | 功能 |
|---|---|
| Z / X | 栏数 −1 / +1 步进（下界 2，2 栏再减 = 退出对比；上界 6；布局 2=1×2、3=1×3、4=2×2、5=3+2、6=3×2） |
| 1 ~ 5 / Q | 评主栏星 / 清除主栏评级（主栏 = 最后点击的栏，与图片模式同语义） |
| PgUp / PgDn | 整组滑动：全部栏一起换成列表里的上/下一组（6 栏满时翻页即整组换血） |
| ← / → | 会话含多帧 raw dump = 帧游标 ±1（各栏按自身帧数钳制，不要求帧数相同）；纯静态图 = 组滑动兜底 |
| 单击某栏 | 设为主栏（full 高清档升级跟随最后交互的栏） |
| + / − / Home | 共享放大 / 缩小 / 回适应窗口（×1.25 步进） |
| 鼠标拖拽 | 共享平移（各栏同点对齐，不同分辨率图内容对齐） |
| 鼠标滚轮（适应窗口态） | 整组滑动（与翻页同款节流） |
| 鼠标滚轮（缩放态）或 Ctrl + 滚轮 | 以光标所在栏为锚共享缩放 |

对比视图下 mpv 不在场：静态图走图片纹理直显，raw dump 走软解逐帧提取（帧数由文件大小 ÷ 单帧字节自动得出）。非流式包（7z/RAR）中仅主栏触发解压，固态包滑窗会顺带解出其余栏。

滚轮按区域区分：预览窗口内（视频 / 图片）如上表，文件列表区的滚轮是普通滚动列表。搜索框等文本输入框聚焦时，快捷键自动失效，不会误触发。

### 文件列表：多选操作

**播放与选择完全解耦**：普通单击 = 纯播放，不写入选区、不清空选区、也不动锚点——"正在播放"≠"已选中"，选区只由 Ctrl / Shift / Ctrl+A 等显式动作建立；Ctrl/Shift 点击只改选区、不切换播放——多选批量编码时不会点一下就切换播放。

| 操作 | 效果 |
|---|---|
| 单击某行 | 纯播放该行；选区与锚点均保持原样（预览不破坏已选好的批量集合） |
| Ctrl + 单击 | 逐个增减：该行已在选区内则移出，不在则加入；锚点落到该行（锚点唯一建立方式之一） |
| Shift + 单击 | 从锚点到该行整段替换选区（支持反向选择；尚无锚点时单选该行并落地锚点，不播放） |
| Ctrl + A | 全选当前标签页全部条目（搜索框聚焦或密码框 / 配置编辑器 / 编码确认窗在场时不触发） |
| 右键菜单「清除选择」 | 清空选区（连同锚点） |
| 切换压缩包 | 选区自动清空（行下标对新包是错位引用） |

已选行行首带琥珀色大写 `S` 标志（与悬停气泡目录段的琥珀同色系），正在播放行由主题蓝整行高亮——一眼区分（既选中又在播放的行为琥珀 `S` 前缀 + 蓝底，两状态并存可辨）。列表行右键菜单共四项：**编码为视频…** / **对比显示（C）** / **全选（Ctrl+A）** / **清除选择**。

选区的最主要用途有两个：批量「编码为视频」（见下文「Encode」）和多图「对比显示」（见上文「对比模式」）——对比入口按视图序取选中集中合格的静态图 / raw dump（<2 提示不放行，>6 取前 6）。

### 文件列表：高级搜索

搜索框支持表达式语法（悬停 "Search:" 标签也可随时查看此表）：

| 语法 | 含义 | 示例 |
|---|---|---|
| 空格 | 与（同时包含） | `aem num` |
| `\|` | 或（任一包含） | `aem\|tnr` |
| `!` | 非（排除） | `report !temp` |
| `"..."` | 精确短语（按原样含空格匹配） | `"new file"` |
| `*` / `?` | 通配符（多字符 / 单字符，不跨空格） | `2026*`、`file?.txt` |
| `<...>` | 分组（改变优先级） | `aem <num\|mode>`、`!<aem\|tnr>` |

优先级：精确短语 > `!` > 空格（与）> `|`（或）。组合示例：`aem num !temp` = 同时含 aem 和 num 且不含 temp；`aem <num|mode> !temp` = 含 aem 且（含 num 或 mode）且不含 temp。

- 匹配不区分大小写；搜索只过滤列表显示，不影响播放
- 过滤在停止输入 0.2 秒后执行（`ui.search_debounce_ms` 可调，0 = 每键即时过滤）
- 输入没打完不报错：`aem !`、`<aem|` 这类残缺输入按"已能解析的部分"过滤；只输 `!` 或 `|` 等纯符号时列表不过滤

### 非标裸数据播放（`[[raw.demux]]` 预设）

无容器封装的裸像素数据（`.yuv` / `.nv12` / 传感器 `*.bin` dump 等）无法凭文件内容识别格式，须命中 `[[raw.demux]]` 预设。**预设命中即可见、即可播**——配置好后操作与普通视频完全相同：出现在视频标签页 → 点击播放 → 进度条 seek 等照常可用。

**内置默认预设（零配置可用）**：程序自带五条默认预设——`*.nv12`→nv12、`*.yuv400`→gray、`*.yuv420`→yuv420p、`*.rgb`→rgb24、`*.raw`→bayer_auto16le + bit_shift=4（fps 均默认 3）。注意：只要在 config.toml 写了任何 `[[raw.demux]]`，即整体替换默认五条（TOML 数组语义）；新文件用默认模板生成时五条已写入并带注释说明。

配置写法（`[raw]` 节，可写任意多条预设）：

```toml
[raw]
# 非标裸数据（raw dump）：没有扩展名清单——文件必须在下方 [[raw.demux]]
# 命中预设才可见、可播放（预设即参数来源：像素格式 + 宽高；帧数由文件
# 大小自动得出，大小必须能被单帧大小整除，否则报错）。
# 可写任意多条预设，按声明顺序取首个 match 命中（先写具体、后写宽泛）。
# fps 可省略，默认 3。
# 常用 pixfmt 速查（完整 195 项名单见文末「附录：raw 格式表」）：
#   YUV420 8bit   : yuv420p / nv12 / nv21 / yuvj420p
#   YUV420 10bit+ : yuv420p10le / p010le / yuv420p16le
#   unpacked Bayer（16bit/像素，即 2 字节）:
#     bayer_rggb16le / bayer_bggr16le / bayer_gbrg16le / bayer_grbg16le
#   Bayer 自动排列探测（伪格式，仅播放链路；encode 不支持）:
#     bayer_auto16le —— 16bit 容器（可配 bit_shift），装载前读首帧
#     按 G 同色对角相关性自动判定 RGGB/BGGR/GBRG/GRBG 排列
# 预设示例（unpacked bayer 的 .bin + NV12 的 .yuv），按需去掉行首 # 启用或仿写：
# [[raw.demux]]
# match = "sensor_a_*.bin"    # 通配 * 与 ?；写文件名（不含路径）
# pixfmt = "bayer_rggb16le"   # FFmpeg 像素格式名（如 yuv420p / nv12 / gray）
# width = 1920                # 可省略或 0：从文件名里的 WxH 字样兜底
# height = 1080               # （如 fpga_in_s0_1920x1080_00_12345.raw）
# fps = 30
# bit_shift = 4               # 可选（默认 0）：12bit 有效数据低对齐存 16bit
#                             # 容器时每 u16 采样左移位数（10bit 填 6）——
#                             # FFmpeg bayer16 转 RGB 恒取高 8 位，低对齐数据
#                             # 只剩 16 级灰阶（播放发黑）；>0 时 pixfmt 须
#                             # le 结尾，快慢路径均生效（7Z/RAR 落盘经单文件
#                             # VFS 借道流层位移，缓存文件保持忠实拷贝，
#                             # 详见 ARCHITECTURE §13.6）
# [[raw.demux]]
# match = "*.yuv"
# pixfmt = "nv12"
# width = 1280
# height = 720
# fps = 25
```

预设字段：`match`（文件名通配）→ `pixfmt`（像素格式，可选值见文末「附录：raw 格式表」）+ `width` / `height`（帧宽高，**可省略或 0**：从文件名中的 `WxH` 字样兜底，多个候选逐一尝试取首个整除通过者）+ `fps`（可省略，默认 3）+ `bit_shift`（可选，默认 0）。

需要知道的规则：

| 事项 | 说明 |
|---|---|
| 可见性唯一入口 | `[raw]` 节没有扩展名清单：文件名命中任一预设的 `match` 才会出现在视频列表；不写预设则仅默认五条（`*.nv12` / `*.yuv400` / `*.yuv420` / `*.rgb` / `*.raw`）可见，其余扩展名不进列表（避免"可见但不可播"的中间态）。把非标扩展名手动加进 `media.video_extensions` 不走 raw 链路——按普通文件交给 mpv，装载失败顶栏报错 |
| 匹配规则 | `match` 只匹配文件名（不含路径），大小写不敏感，支持 `*` / `?`；多条预设按声明顺序取首个命中——先写具体（`sensor_a_*.bin`）后写宽泛（`*.yuv`） |
| 全入口生效 | ZIP 流式、7Z / RAR（后台解压到缓存后播放）、目录模式（拖入文件夹，命中预设的裸数据与普通媒体一起收集进列表）均可播放 |
| 参数与帧数 | 像素格式 / fps 由预设提供（fps 默认 3）；宽高可用预设值，预设省略/为 0 时从文件名里的 `WxH` 字样兜底（如 `fpga_in_s0_1920x1080_00_12345.raw`），两处都无则报错；文件名含多个 `WxH` 时逐一尝试，预设有效值优先；帧数自动得出 = 文件大小 ÷ 单帧字节，无需配置 |
| 装载前四项校验 | 配置错误直接顶栏报错并附核对指引，不会白解压也不会播出错位画面：① `pixfmt` 不在支持名单（195 项 + 伪格式 bayer_auto16le，见文末「附录：raw 格式表」）；② 宽高无法确定（预设与文件名兜底均无有效 `WxH`）；③ 文件大小不能被任一候选分辨率的单帧字节数（宽×高×位数÷8）整除——尾部零头会造成画面错位，前置拦截；④ `bit_shift` 越界（>8）或 >0 而 `pixfmt` 非 `le` 结尾。7Z / RAR 在解压之前即拦截配置错误 |
| bit_shift 播放发黑修正 | 12bit 等低对齐数据存 16bit 容器时画面发黑（只剩 16 级灰阶）：设 `bit_shift = 4`（12bit）/ `6`（10bit）左移修正。快慢路径均生效——ZIP 流式 / 目录模式在流层位移；7Z / RAR 落盘经单文件 VFS 借道同一流层实现（位移不落盘，缓存文件保持忠实拷贝），详见 ARCHITECTURE §13.6 |
| 失败回显 | mpv 实际装载失败（文件损坏等）顶栏报错——普通损坏视频同样受益，不再黑屏无提示；raw 文件的报错附「核对 [[raw.demux]] 预设」提示 |
| 编码联动 | 命中预设的文件可右键「编码为视频…」或走 `archview.exe encode`，族判定与帧率参数同源自预设（见下文「Encode」节） |

### 缓存管理（`[cache]` 节）

7Z / RAR 走"后台解压到磁盘缓存再播放"的慢路径；缓存目录、容量与清理行为全部可配：

```toml
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
```

| 配置项 | 说明 |
|---|---|
| `cache.dll_dir` | libmpv-2.dll 搜索目录，`auto` = exe 同目录 |
| `cache.dir` | 7Z/RAR 解压缓存目录，`auto` = `%TEMP%\archview_cache`；可指向 RAM Disk 提速（见下方"RAM Disk 缓存"）；目录不存在会自动创建 |
| `cache.max_size_mb` | 缓存总量上限（MB），超出按 LRU（最久未访问）驱逐 |
| `cache.chunk_size_mb` | ZIP 流式播放的解码块大小（MB） |
| `cache.chunk_cache_blocks` | 解码块 LRU 缓存块数；ZIP 流式播放的内存占用 ≈ 两者乘积（默认 4 × 64 = 256MB） |
| `cache.clean_on_exit` | 退出程序时自动清空解压缓存（默认 false） |
| `cache.clean_on_switch` | 切换压缩包时是否清理上一个包的缓存（默认 false，保留以便切回时命中） |

清理时机：

1. **手动**：顶栏「清理缓存」按钮，显示释放空间；「缓存目录」按钮用资源管理器打开。
2. **切换压缩包**：`clean_on_switch` 为 true 时删除上一个包的解压缓存（按 archive_id 前缀匹配）。
3. **退出程序**：`clean_on_exit` 为 true 时清空整个缓存目录。
4. **容量上限**：超过 `max_size_mb` 时 LRU 驱逐最旧的文件。
5. **启动**：清理上次中断解压留下的 `.part*` 半截文件。

缓存按压缩包路径稳定命名：同一包 A→B→A 切回（或程序重启后重开）缓存直接命中，无需重新解压。

#### 进阶：RAM Disk 缓存（可选推荐）

缓存落在机械盘上时首次播放等待明显。把 `cache.dir` 指向 RAM Disk 可以显著加速，且**断电/重启即自动清空**，无需维护：

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

### 播放器配置项（`[media]` / `[raw]` / `[player]` / `[ui]` / `[image]` / `[extract]` 节）

```toml
[media]
# 视频 / 图片的文件扩展名识别列表（不区分大小写）
video_extensions = ["mp4", "mkv", "avi", "mov", "flv", "ts", "webm", "wmv", "m4v"]
# 相机 RAW（内嵌预览直读）：Sony ARW 家族 + Nikon NEF 家族
image_extensions = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff",
                    "arw", "nef", "nrw", "srf", "sr2", "arq"]
# 打开单个媒体文件（拖入文件/发送到/文件菜单「打开媒体文件」）时，对其
# 所在目录的递归收集深度：1 = 只收集同级媒体文件（默认）；N = 向下 N 层；
# 0 = 不限制（与「打开媒体目录」一致）
file_open_depth = 1

[raw]
# 非标裸数据（raw dump）预设：命中 [[raw.demux]] 才可见、可播放；
# 宽高可省略（或 0）= 从文件名内嵌 WxH 兜底，fps 可省略（默认 3）。
# 内置五条默认预设如下（可改可删；在 config.toml 写任何 [[raw.demux]]
# 即整体替换默认五条），完整写法与校验规则见「非标裸数据播放」
[[raw.demux]]
match = "*.nv12"
pixfmt = "nv12"

[[raw.demux]]
match = "*.yuv400"
pixfmt = "gray"

[[raw.demux]]
match = "*.yuv420"
pixfmt = "yuv420p"

[[raw.demux]]
match = "*.rgb"
pixfmt = "rgb24"

[[raw.demux]]
match = "*.raw"
pixfmt = "bayer_auto16le"   # 伪格式：装载时读首帧自动探测 Bayer 排列
bit_shift = 4               # 12bit 低对齐存 16bit 容器的左移修正

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
# 文件列表搜索防抖（毫秒）：停止输入该时长后才执行一次过滤；0 = 每键即时过滤
search_debounce_ms = 200
# 解压进度覆盖层显示前的宽限期（毫秒）：快速解压全程不打扰播放
extract_overlay_delay_ms = 500
# UI 重绘频率三档（fps），钳制 5~240：悬停 / 拖拽 / 其余情况
redraw_hover_fps = 30
redraw_drag_fps = 60
redraw_other_fps = 60

[image]
# 图片 egui 纹理直显（后台解码直接上纹理，翻页秒开；解码失败自动回退 mpv）
image_texture = true
# 图片纹理 LRU 缓存张数（含当前张）：预解码相邻页，翻页零等待
image_cache_count = 3
# 原图缓存（orig-lru）总预算（MB，钳 64~4096）：像素探针 / full 升级复用 /
# [ ] 亮度毫秒级重建三个消费者共用；单张准入 = 预算/2，张数上限 = 当前栏数+2
image_orig_cache_mb = 512
# 基础纹理档尺寸上限；full 高清档升级资格判据同此值
image_max_dim = 4096
# 高清档触发阈值：zoom×ppp 超过此值才解码/换画 full 级纹理
image_full_threshold = 2.0
# 高清档放大前原图级观感增强（保边降噪 → unsharp 锐化），总开关
image_full_sharpen = true
# 锐化强度 amount 与半径 σ（决定强化频带）
image_full_sharpen_amount = 1.0
image_full_sharpen_sigma = 1.5
# 白边抑制折点曲线 [shoot_v, ratio]（空数组 = 关闭，零开销）
image_full_sharpen_white_edge_curve = [[100.0, 1.0], [150.0, 0.9], [200.0, 0.6], [240.0, 0.3], [255.0, 0.05]]
# 锐化前保边降噪 + 算法切换（guided / bilateral，参数独立、强度同量纲可对比）
image_full_denoise = true
image_full_denoise_algo = "guided"
image_full_bilateral_strength = 10.0
image_full_guided_strength = 10.0
image_full_guided_radius = 1

[extract]
# solid 包解压时顺带预取后续文件的总大小上限（MB，下游预取），0 = 关闭
prefetch_downstream_mb = 512
# 上游顺带落盘最大上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置
# 条目（7z 限于目标所在 folder 内、RAR 为整个前缀）顺带写盘，随机跳看后
# 往回翻直接命中缓存。解码零增量，仅占磁盘写入量。只是上限非目标——实取
# 取决于免费区（folder 前缀）大小，通常小于该值（下游 prefetch_downstream_mb
# 则尽量取满）
prefetch_upstream_max_mb = 512
```

| 配置项 | 说明 |
|---|---|
| `media.*_extensions` | 视频 / 图片的文件扩展名识别列表（不区分大小写）；图片列表含相机 RAW（`arw` / `nef` 家族，内嵌预览直读） |
| `media.file_open_depth` | 打开单个媒体文件（拖入 / 发送到 / 文件菜单「打开媒体文件」）时对其所在目录的递归收集深度：`1` = 只收集同级媒体文件（默认，防盘根/超大目录全盘扫描）；`N` = 向下 N 层；`0` = 不限制（与「打开媒体目录」一致） |
| `raw.demux` | 非标裸数据预设数组（`[[raw.demux]]`）：`match` 文件名通配 + `pixfmt` + 可选 `width`/`height`/`fps`/`bit_shift`；宽高省略或 0 时从文件名内嵌 `WxH` 兜底。内置五条默认（`*.nv12`→nv12、`*.yuv400`→gray、`*.yuv420`→yuv420p、`*.rgb`→rgb24、`*.raw`→bayer_auto16le+bit_shift=4）；config.toml 写任何 `[[raw.demux]]` 即整体替换默认五条。详见「非标裸数据播放」 |
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
| `ui.search_debounce_ms` | 文件列表搜索防抖（毫秒）：停止输入该时长后才执行一次过滤重建（默认 200）。0 = 不防抖，每键即时过滤——小包最顺手，万级大包建议保持默认 |
| `ui.extract_overlay_delay_ms` | 解压进度覆盖层的宽限期（毫秒）：播放中切到需解压的条目，超过该时长仍未解完才隐藏视频显示进度；快速解压全程不打扰播放 |
| `ui.redraw_hover_fps` / `redraw_drag_fps` / `redraw_other_fps` | UI 重绘频率三档上限（fps，钳 5~240）：悬停/滚轮、拖拽中、其余情况（解码完成显示 / 解压进度刷新等）。静置/悬停时 CPU/GPU 占用可控，弱机可下调；视频播放不受影响（vsync 主钟） |
| `image.image_texture` | 图片走 egui 纹理直显（后台解码直接上纹理，翻页秒开；静图与动图 gif 均走此链路）。解码失败的图片自动回退 mpv |
| `image.image_cache_count` | 图片纹理 LRU 缓存张数（含当前张），预解码 ±1 相邻页，翻页零等待 |
| `image.image_orig_cache_mb` | 原图缓存（orig-lru）总预算（MB，钳 64~4096，默认 512）：解码后的原图 CPU 副本，供像素探针（Alt+左键）/ full 高清档升级复用 / `W/S` 亮度毫秒级重建三个消费者共用。单张准入 = 预算/2（超准入巨图不驻留，探针显示"原图未保留"、亮度重走解码）；张数上限 = 当前栏数+2，逐出最旧但跳过当前显示与对比保护集 |
| `image.image_max_dim` | 基础纹理档尺寸上限（预放大 ×ppp 后超过则原尺寸直进）；full 高清档升级资格判据同此值（详见 [架构设计 13.4 两档策略](doc/架构设计/13_渲染架构迁移.md)） |
| `image.image_full_threshold` | 高清档触发/换画阈值：屏幕物理像素超过基础纹理该倍数（zoom×ppp > 此值）才解码/换画 full 级纹理。full 为 ×3 屏幕密度纹理，默认 2.0 = badge 200% 起接管、300% 处逐纹素 1:1 直出（200%~300% 为轻微缩采样，静止图无感）；调大（如 3.0）甜点后移且基本关闭 |
| `image.image_full_sharpen` | 高清档**放大前**在原图分辨率执行观感增强（保边降噪 → unsharp 锐化，先把边缘坡度变陡再 Lanczos 放大），总开关；噪点同步放大由前置降噪对冲 |
| `image.image_full_sharpen_amount` / `image_full_sharpen_sigma` | unsharp 强度（默认 1.0：越大边缘越脆，>0.8 光晕/振铃明显；控噪优先调 σ/降噪而非减强度）与半径 σ（默认 1.5：决定强化频带，越大越"体积感"、光晕越宽；暗部分层时优先切导向降噪或降 σ） |
| `image.image_full_sharpen_white_edge_curve` | 白边抑制折点曲线 `[shoot_v, ratio]`（默认 `[100,1]→[150,0.9]→[200,0.6]→[240,0.3]→[255,0.05]`，2026-11-18 release 实测定版）：shoot 多尺度方差标出孤立强边（unsharp 白边亮晕来源），分段线性插值成 256 项 LUT 只压正向增强、细纹理全额保留；空数组 = 关闭（连方差计算都跳过）；折点非法回退默认并 WARN |
| `image.image_full_denoise` | 锐化前保边降噪（压平坦区噪点不糊边缘；默认开） |
| `image.image_full_denoise_algo` | 降噪算法 `"guided"`（导向滤波，默认：锯齿最弱、弱纹理保留最好）/ `"bilateral"`（双边 3×3：眼部更锐）；两算法参数独立、strength 同量纲（亮度差容忍级）可同值对比；非法值 WARN 回退导向 |
| `image.image_full_bilateral_strength` | 双边降噪颜色容忍度 σc（默认 10）：5~10 温和保纹理 / 10~20 通用 / 20~50 激进 / >50 ≈ 普通模糊；涂抹感重调小、噪点残留调大 |
| `image.image_full_guided_strength` / `image_full_guided_radius` | 导向降噪结构容忍度 D（默认 10，内部换算 ε=D² 方差阈值：3~8 温和 / 15~30 重噪 / 30~60 激进）与窗口半径（默认 1 = 3×3，人像实测定版；重噪图可 2~4）；涂抹感重调小、噪点残留调大 |
| `extract.prefetch_downstream_mb` | 下游预取上限（MB），0 = 关闭：solid 包（7z / solid RAR）解压时顺带预取下标更大的后续文件，一次顺序解压流解出一批，翻页直接命中 |
| `extract.prefetch_upstream_max_mb` | 上游顺带落盘最大上限（MB），0 = 关闭：定位目标时必然被顺序解掉丢弃的前置条目顺带写盘，随机跳看后回看直接命中缓存；**伴随量非配额**——实取以免费区（folder 前缀）大小为准，通常小于该值（下游 `prefetch_downstream_mb` 则尽量取满）；旧键名不再兼容，旧配置请手动改键名 |
| `io.io_parallel_max_num` | IO 并行槽位数（钳 1~16，默认 1）：图片 base/full 解码、raw 帧软解、raw 直方图、评级探测共用同一槽位预算（三类优先级 解码 > 预取 > 评级）。`1` = 全链串行（机械盘 / NAS 最优：随机读不交错）；SSD 可调 4~6 恢复并发。超出上限的请求排队，完成事件驱动逐个放行。详见 [架构设计 17](doc/架构设计/17_IO调度层.md) |
| `io.prefetch_yield` | 预取让路（默认 true）：有解码在途时预取一律推迟（用户可见内容优先占住 IO，预取在解码间隙渐进推进）；false = 预取与解码并行抢 IO |
| `io.rating_yield` | 评级让路（默认 true）：评级探测为最低优先级——解码/预取排队时让位不派发，在途评级文件读完让出（文件粒度抢占）；槽位有空（total < `io_parallel_max_num`）时借空槽随行（SSD 并行档评级扫描不再饿死）；false = 后台扫（闸门旁路） |

## Encode：帧序列 dump → 视频

raw dump 序列帧后台编码为视频文件，不占用播放。两个入口同一 libmpv 编码引擎，编码参数共用 config.toml 的 `[encode]` 节（默认 `libx264` + `crf 23` + `medium`）：

- **UI 入口**：播放器列表右键「编码为视频…」（见「播放器 → 多选操作」的右键菜单）。
- **CLI 入口**：`archview.exe encode`——批量 / 无显示器 / 脚本场景。

### UI：列表右键「编码为视频…」

1. **对象**：`[[raw.demux]]` 预设命中的裸数据文件（`.yuv` / `.bin` 等，预设写法见「播放器 → 非标裸数据播放」）。右键单行命中预设即可直接发起（选区为空且该行未命中预设时菜单项置灰——"正在播放"不算已选，不会误亮编码入口）；多选的行须全部属于同一预设族——混入其它文件（或未命中预设的行）时**不开窗，顶栏红字报错**，不会编出错位画面。
2. **范围**：右键单个 raw 文件时，确认窗可二选一「整个序列（N 帧）」（全包内同一预设族的全部文件，自然序）或「仅此文件」；多选发起则为精确模式——选多少编多少（忽略右键行，按文件名自然序），帧序号有缺口时确认窗给出琥珀色警告。
3. **确认窗**：显示文件数 / 命中预设 / 分辨率 / 像素格式；帧率默认来自预设、可直接改（1–240 fps）；输出路径可编辑（默认输出到压缩包所在目录），**输出文件已存在时拒绝覆盖**（红字提示，改路径才能继续）。
4. **编码中**：任务在后台线程执行，**不占用播放**——期间可继续浏览列表、播放其它媒体；进度窗显示输出路径与已运行时长，可随时「取消编码」。同一时刻只允许一个编码任务，进行中再发起会顶栏提示。
5. **完成**：顶部绿条显示总帧数与耗时，**点击绿条打开输出所在目录**。

### CLI：`archview.exe encode`

```bat
archview.exe encode <输入...> [选项]
```

输入三种形式（**不可混用**）：

- **帧文件**：一个或多个 dump 文件（须同一目录）= 精确模式，所见即所选；多文件视为一个序列的成员，按自然序（`1/2/10`）排序后编码
- **目录**：目录下恰好一族 `[[raw.demux]]` 命中的文件 → 全族编码；多族时报错并列各族清单（选中其中一族任一帧文件重发即可）
- **压缩包**：zip / 7z / rar——包内条目名匹配预设，**免解压直编**

| 选项 | 说明 |
|---|---|
| `-o, --out <文件>` | 输出路径（容器由扩展名推断，如 `.mp4` / `.mkv`；无扩展名自动补 `.mp4`）。缺省 = 序列所在目录 + 首帧基名（`dump_0000.nv12` → `dump_0000.mp4`）；输出已存在**拒绝覆盖** |
| `-p, --password <pw>` | 加密包密码（可多次指定按序尝试；config.toml 顶层 `passwords` 列表兜底） |
| `-q, --quiet` | 静默：不弹结果窗（有控制台时仍打印） |

行为要点：

- 帧率取自命中的 `[[raw.demux]]` 预设；编号不连续只警告不中止（也是「发送到」多文件受命令行 32767 字符上限截断的防呆提示——丢尾帧会被缺口警告暴露出来）。
- GUI 子系统 exe：从终端启动自动附加父控制台打印进度；从资源管理器发起时以结果弹窗收尾（`-q` 关闭弹窗）。
- 退出码：0 = 完成；1 = 编码失败；2 = 用法/输入错误。
- 提示：大序列请发目录——「发送到」多文件受命令行长度上限约束，超限时 explorer 可能静默截断。

### 编码配置项（`[encode]` 节）

```toml
[encode]
# 「编码为视频」（UI 列表右键）的编码参数。vcodec 需本机 libmpv 构建内置
# （libx265 / libsvtav1 等视构建而定）；ovcopts 为附加编码器选项
# （如 "bf=3" / "threads=8"），同键覆盖 crf/preset 组装的默认项
vcodec = "libx264"
# 恒定质量（x264/x265 crf）：越低画质越高体积越大
crf = 23
# 编码速度预设（ultrafast…veryslow）
preset = "medium"
# 附加编码器选项；空 = 无
ovcopts = ""
```

| 配置项 | 说明 |
|---|---|
| `[encode]` | 「编码为视频」的编码参数：`vcodec`（编码器，默认 `libx264`；`libx265` / `libsvtav1` 等需本机 libmpv 构建内置）+ `crf`（恒定质量，越低画质越高体积越大，默认 23）+ `preset`（编码速度预设 `ultrafast`…`veryslow`，默认 `medium`）+ `ovcopts`（附加编码器选项，如 `"bf=3"` / `"threads=8"`，同键覆盖 crf/preset 组装的默认项） |

## VarTidy：var 资源包增量整理（CLI 子命令）

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

### VarTidy 配置项（`[vartidy]` 节）

```toml
[vartidy]
# var 仓库根目录（archview.exe tidy 用；整理器绝不修改仓库已有内容，仅增量合入）
repo_dir = ""
# 报告输出目录；"auto" = 首个输入压缩包所在目录
report_dir = "auto"
```

| 配置项 | 说明 |
|---|---|
| `vartidy.repo_dir` | var 仓库目录，必须已存在（整理器绝不修改仓库已有内容）；空 = 未配置，CLI 需 `-o` 指定；详见 [架构设计第 12 章](doc/架构设计/12_VarTidy整理器.md) |
| `vartidy.report_dir` | 报告输出目录，`auto` = 首个输入压缩包所在目录（不写入仓库，保持仓库纯净） |

## 从源码构建

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

## 已知限制与规划

- 分卷 ZIP：暂不支持（分卷 RAR 已支持）。
- 兜底代码页只用一个配置编码（默认 GBK/简中）：密码侧只追加一个候选；zip 文件名侧 UTF-8 校验失败后也只用它解码。包若在日文等其它代码页系统上创建，需把 `codepage` 改为对应编码（如 `shift_jis`）。GBK/Shift-JIS 等双字节编码之间无法从字节层自动区分。
- 视频缩略图、播放进度记忆：规划中。
- raw dump（`[[raw.demux]]`）的宽高与像素格式需按文件名预设，不能按文件内容自动推算：列为后续增强。

## 更多文档

架构与开发文档按节拆分在 `doc/` 目录下，按需阅读：

- [doc/架构设计/](doc/架构设计/README.md)：架构设计（原 ARCHITECTURE.md）——VFS 抽象层、双路径播放、密码链路、缓存策略、线程模型、模块清单、大包性能设计。
- [doc/架构落地开发/](doc/架构落地开发/README.md)：修复记录 / 踩坑手册 / 日志排查对照 / 测试运行方式与覆盖矩阵（原 DEVLOG.md）。

## 开源协议

[MIT](LICENSE)。注意：本程序运行时动态加载的 libmpv 以其自身的开源协议（GPL/LGPL）分发，dll 需自行获取（见上文"获取 libmpv-2.dll"）。

## 附录：raw 格式表（`[[raw.demux]]` pixfmt 可选值）

`[[raw.demux]]` 的 `pixfmt` 可选值共 **195 项**（大小写不敏感），名单同 `ffmpeg -pix_fmts`（ffmpeg 7.x）筛除硬件 / 调色板 / bitstream / 无输入端口项——与 mpv `demuxer-rawvideo-mp-format` 接受的同一名单。

**伪格式 `bayer_auto16le`**（不在 195 项名单内）：16bit 容器 Bayer 的自动排列探测——装载前读首帧，按 G 同色沿对角的相关性自动判定 RGGB / BGGB / GBRG / GRBG 四种排列并替换为对应的具体 `bayer_*16le` 格式；探测置信不足（近纯灰、过小帧、R/B 不可分）时兜底 `bayer_rggb16le`。**仅播放链路生效**（ZIP 流式 / 目录 / 7z / RAR 均支持）；encode 编码引擎显式拒绝（要求写明具体排列）。可配合 `bit_shift` 使用（探测在位移修正前的原始数据上进行）。

**位数 = 每像素容器位数**（不是逻辑位数）：9~16bit 采样实际占 2 字节容器、rgb555 / rgb444 填零到 16bit 等，帧字节数 = `width × height × 位数 ÷ 8`。装载前的整除校验以此为基，才能与文件实际大小吻合（如 `yuv420p10le` 每像素 24bit 而非逻辑 15bit）。

不在表内的名字（如 MIPI packed bayer `mipi_rggb10`、调色板 `pal8`）会报「不支持的格式」。

| 家族 | 位数/px | 格式名 |
|---|---|---|
| Bayer 8-bit | 8 | `bayer_bggr8` `bayer_rggb8` `bayer_gbrg8` `bayer_grbg8` |
| Bayer 16-bit | 16 | `bayer_bggr16le` `bayer_bggr16be` `bayer_rggb16le` `bayer_rggb16be` `bayer_gbrg16le` `bayer_gbrg16be` `bayer_grbg16le` `bayer_grbg16be` |
| Gray 8-bit | 8 | `gray` |
| Gray 9~16-bit | 16 | `gray9le` `gray9be` `gray10le` `gray10be` `gray12le` `gray12be` `gray14le` `gray14be` `gray16le` `gray16be` |
| Gray float | 32 | `grayf32le` `grayf32be` |
| Gray+Alpha | 16 / 32 | `ya8`（16）；`ya16le` `ya16be`（32） |
| YUV planar 8-bit | 9 | `yuv410p` |
| YUV planar 8-bit | 12 | `yuv420p` `yuv411p` `yuvj420p` `yuvj411p` |
| YUV planar 8-bit | 16 | `yuv422p` `yuv440p` `yuvj422p` `yuvj440p` |
| YUV planar 8-bit | 20 | `yuva420p` |
| YUV planar 8-bit | 24 | `yuv444p` `yuvj444p` `yuva422p` |
| YUV planar 8-bit | 32 | `yuva444p` |
| YUV420p 9~16-bit | 24 | `yuv420p9le` `yuv420p9be` `yuv420p10le` `yuv420p10be` `yuv420p12le` `yuv420p12be` `yuv420p14le` `yuv420p14be` `yuv420p16le` `yuv420p16be` |
| YUV440p 10/12-bit | 24 | `yuv440p10le` `yuv440p10be` `yuv440p12le` `yuv440p12be` |
| YUVA420p 9~16-bit | 40 | `yuva420p9le` `yuva420p9be` `yuva420p10le` `yuva420p10be` `yuva420p16le` `yuva420p16be` |
| YUV422p 9~16-bit | 32 | `yuv422p9le` `yuv422p9be` `yuv422p10le` `yuv422p10be` `yuv422p12le` `yuv422p12be` `yuv422p14le` `yuv422p14be` `yuv422p16le` `yuv422p16be` |
| YUVA422p 9~16-bit | 48 | `yuv422p9le` `yuv422p9be` `yuv422p10le` `yuv422p10be` `yuv422p12le` `yuv422p12be` `yuv422p14le` `yuv422p14be` `yuv422p16le` `yuv422p16be` |
| YUV444p 9~16-bit | 48 | `yuv444p9le` `yuv444p9be` `yuv444p10le` `yuv444p10be` `yuv444p12le` `yuv444p12be` `yuv444p14le` `yuv444p14be` `yuv444p16le` `yuv444p16be` |
| YUVA444p 9~16-bit | 64 | `yuv444p9le` `yuv444p9be` `yuv444p10le` `yuv444p10be` `yuv444p12le` `yuv444p12be` `yuv444p14le` `yuv444p14be` `yuv444p16le` `yuv444p16be` |
| NV12/21 | 12 | `nv12` `nv21` |
| NV16 | 16 | `nv16` |
| NV24/42 | 24 | `nv24` `nv42` |
| P010/P012/P016 | 24 | `p010le` `p010be` `p012le` `p012be` `p016le` `p016be` |
| P210/P212/P216 | 32 | `p210le` `p210be` `p212le` `p212be` `p216le` `p216be` |
| P410/P412/P416 | 48 | `p410le` `p410be` `p412le` `p412be` `p416le` `p416be` |
| YUV packed 16-bit | 16 | `yuyv422` `uyvy422` `yvyu422` |
| YUV packed 24/32-bit | 24 / 32 | `vuyx`（24）；`vuya`（32） |
| YUV packed 32-bit | 32 | `y210le` `y212le` `xv30le` |
| YUV packed 64-bit | 64 | `ayuv64le` `xv36le` |
| RGB 24-bit | 24 | `rgb24` `bgr24` `0rgb` `rgb0` `0bgr` `bgr0` |
| RGBA | 32 | `argb` `rgba` `abgr` `bgra` |
| RGB 8-bit 索引 | 8 | `rgb8` `bgr8` `rgb4_byte` `bgr4_byte` |
| RGB 565 | 16 | `rgb565le` `rgb565be` `bgr565le` `bgr565be` |
| RGB 555 | 16 | `rgb555le` `rgb555be` `bgr555le` `bgr555be` |
| RGB 444 | 16 | `rgb444le` `rgb444be` `bgr444le` `bgr444be` |
| RGB 48-bit | 48 | `rgb48le` `rgb48be` `bgr48le` `bgr48be` |
| RGBA 64-bit | 64 | `rgba64le` `rgba64be` `bgra64le` `bgra64be` |
| RGB 10-bit | 32 | `x2rgb10le` `x2bgr10le` |
| RGBA float16 | 64 | `rgbaf16le` `rgbaf16be` |
| GBRP 8-bit | 24 | `gbrp` |
| GBRP 9~16-bit | 48 | `gbrp9le` `gbrp9be` `gbrp10le` `gbrp10be` `gbrp12le` `gbrp12be` `gbrp14le` `gbrp14be` `gbrp16le` `gbrp16be` |
| GBRP float | 96 | `gbrpf32le` `gbrpf32be` |
| GBRA 8-bit | 32 | `gbrap` |
| GBRA 10~16-bit | 64 | `gbrap10le` `gbrap10be` `gbrap12le` `gbrap12be` `gbrap14le` `gbrap14be` `gbrap16le` `gbrap16be` |
| GBRA float | 128 | `gbrapf32le` `gbrapf32be` |
| XYZ | 48 | `xyz12le` `xyz12be` |

常用速查：摄像头裸流 `nv12` / `yuv420p`（12bit/px）；unpacked bayer 传感器 `bayer_rggb16le`（16bit/px）；灰度 `gray`（8）/ `gray16le`（16）；屏录 RGB `bgra`（32）/ `rgb24`（24）。
