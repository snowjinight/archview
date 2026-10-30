# DEVLOG — 修复记录 / 需求变更细节 / 踩坑手册

> 定位：[ARCHITECTURE.md](ARCHITECTURE.md) 的**细节篇**。架构文档只保留宏观内容
> （机制设计、技术决策、数据流、不变量）；本文承接四类细节：
> **修复记录**（现象 → 根因 → 修复 → 教训）、**踩坑点**（带回归测试钉死）、
> **排查手册**（日志关键词对照，现场 grep 对号入座）、
> **测试方法**（运行方式、覆盖矩阵、headless e2e 原理）。
>
> 交叉引用约定：架构文档引本文用 `DEVLOG §N`，本文引架构用 `ARCHITECTURE §N`。
> 维护约定：新的修复/坑点先落本文档对应专题（无则新增专题），架构文档只在机制
> 变化时更新并留一行指引。

## 目录

| 专题 | 内容 | 对应架构章节 |
|---|---|---|
| §1 | 大包性能专项修复（Phase 2）：folder 直达 / ZIP 头 / 虚拟列表四坑 | ARCHITECTURE §11 |
| §2 | 渲染迁移与图片浏览器踩坑（九坑） | ARCHITECTURE §13 / §13.4 |
| §3 | 播放流畅性专项：mpv core 锁三轮攻坚（打开冻结 → 切换卡顿 → seek 卡顿） | ARCHITECTURE §13.2 |
| §4 | 点击状态机排查手册（start_extract 分支链与日志对照） | ARCHITECTURE §11.4 |
| §5 | 解压会话化回归测试细节（单测 + UI 冒烟） | ARCHITECTURE §11.4 |
| §6 | 测试运行方式与覆盖矩阵（headless e2e 原理、外部依赖、VarTidy 冒烟环境） | 全项目 |

---

## 1. 大包性能专项修复（Phase 2：27G ZIP / 56G 7z / 万级条目）

> 机制设计（批量解压摊薄、上下游预取、会话化）见 ARCHITECTURE §11.1/§11.4；
> 本节只记事故驱动的修复叙事与坑。

### 1.1 7z folder 直达（最重要的一次修复）

- **现象**：56G 固态 7z 包，解压第 417 个条目（仅一张 JPG）卡 10+ 分钟，CPU 持续满载——表象酷似死循环。
- **根因**：sevenz-rust 的 `SevenZReader::for_each_entries` **从 folder 0 开始顺序解压全部前置 folder**。
  7z 的 folder（固态块）彼此独立，每个 folder 的 pack 流偏移记录在头信息里，但该 API 不利用它。
- **修复**：改用 `BlockDecoder::new(folder_index, ...)` —— 构造时直接 seek 到目标 folder 的 pack 流偏移，
  **前置 folder 只 seek 不解压**。目标即使排在第 4 万个条目，也只需顺序解压其所在 folder 内
  目标之前的少量数据。10 分钟级 → 秒级。
- **教训**：读依赖库源码确认其遍历策略，"能跑通"的 API 路径可能隐含 O(全包) 成本。

### 1.2 ZIP 中央目录逐字段偏移写错

- 中央目录（CD）逐字段偏移曾被写错（compressed/uncompressed size、name/extra/comment length
  共 6 个字段），导致 fast path 全挂、退回慢路径逐条试探——万级条目下就是"打开卡十几分钟"。
  修复后单次顺序读 CD 即完成列表（NAS 随机读 → 顺序读，分钟级 → 秒级）。
- ZIP64 支持：offset/size 超 32 位时的扩展字段解析。

### 1.3 取消机制必须打日志（否则表现为"幽灵并行解压"）

- 取消机制：`Arc<AtomicBool>`。批外跳转的"取消重起"后被解压会话化取代
  （ARCHITECTURE §11.4），cancel 只剩一个用途：切换压缩包时终止解码线程。
- **坑**：所有取消点必须打日志——否则表现为"多条后台解压日志但看不到打断"
  （实际是缓存命中路径丢弃任务却没置 cancel 标志，旧线程静默跑完整个批，
  与新任务并行抢 NAS 带宽）。

### 1.4 行列表虚拟化四坑（每一条都有回归测试钉死）

**机制设计**（三层：行数据缓存一次性构建 / `show_viewport` 只渲染可视行 ±3 /
首帧估算行高 + 实测反馈）见 ARCHITECTURE §11.2。

| 坑 | 现象 | 修复 |
|---|---|---|
| **usize 下溢** | 手动向列表上方滚动再点“下一个” → 列表直接跳到底部。目标行在渲染范围之前时 `pos - range.start` 下溢，debug 下 panic，**release 下 wrap 成 2^64 级天文数字** → 滚动目标被钳到 max_offset | 一切虚拟化下标换算用 `isize` |
| **行高不均匀** | 长文件名自动换行把行撑成 2 行高，首行实测高度被钉在 2 行值上 → 渲染范围按虚高缩水 → 底部空隙 + 翻页时行高双稳态翻转 | 所有列表行 `.truncate()` 强制单行；完整路径走悬停气泡（wid 子窗口时代气泡被 mpv 原生窗口遮挡而禁用，render API 单平面后恢复；目录段琥珀色/文件名主题蓝分色，LayoutJob 单 Label 保证长路径换行正常） |
| **scroll_to_rect 坐标系** | 高亮追踪滚动跳到错误位置。`scroll_to_rect` 接收**屏幕坐标**，而虚拟化闭包内 `max_rect().top()` = 内容顶 + min_row×step，需按此换算目标行 y | 换算公式 `(pos - range.start) × step + 闭包 max_rect().top()` |
| **每帧重算** | 万级条目每帧过滤/排序/clone，UI 帧率跌到个位数 | 行缓存一次性构建；排序预生成小写键再按自然序比较（避免比较器内每次 2 次 to_lowercase 分配）；实测 7-Zip 打包时无视添加顺序强制按字典序写入，故 `ui.sort_files` 默认 false（包内顺序 = 固态包物理码流顺序，预取最优），true = 自然序（数字段按数值 1/2/10）供漫画/图集场景 |

### 1.5 多帧测试方法论

虚拟化 bug 大多是跨帧时序问题（滚轮惯性、动画收敛、行高反馈延迟），
单帧单 pass 的 egui 测试 (`__run_test_ui`) 复现不了。用 `ctx.run_ui(RawInput)` 手动喂
PointerMoved/MouseWheel 事件序列跑 100+ 帧，记录每帧 `ScrollArea::state.offset` 轨迹断言——
上面的 usize 下溢 bug 就是这么先复现再修复的。

---

## 2. 渲染迁移与图片浏览器踩坑（本次迁移实际炸过的雷）

> 迁移机制（glow 链路、PaintCallback blit、两档纹理体系、zoom 语义）见
> ARCHITECTURE §13.2/§13.4；本节为九坑完整记录。

### 坑 1：手写 FFI 签名错 → 运行时 0xc000041D 崩溃

`mpv_render_context_create` 的出参 `res` 是第一参数（不是指针垫在最后），
`mpv_render_update_fn` 返回 `void`（不是 u64）。编译器只校验"调用处 vs 我们的
声明"，声明与调用**一致地错**可完全编译通过——这是手写 FFI 的固有盲区；且 53 个
既有测试全部走 `vo=null` 无头管线或纯 Rust 逻辑，根本到不了 render 路径，静默
通过。预防：bindgen 生成绑定，或至少加 GUI 冒烟脚本（启动 exe → 等 5s → 查存活）。

### 坑 2：blit 源跟随 READ 绑定 → "套娃虚影 + 疯狂闪烁"

现象：播放时视频区出现逐级缩小、逐级右移的进度条/时间/暂停按钮虚影，全部同步
移动，画面疯狂闪烁；暂停后虚影全消。根因链条：

- `glBlitFramebuffer` 的 src 由 **READ 绑定**决定，`mpv_render_context_render`
  内部渲染可能把 READ 留在别处（如屏幕）→ src(0,0,w,h) 实际读到**窗口左下角**
  （正是左侧列表底部 + 控制条左段）；
- 每帧执行 = 屏幕自我迭代拷贝 → 内容逐级缩小右移（套娃），且每帧实时重拷 → 同步
  移动 + 闪烁；
- 暂停后不再调 render、READ 保持上帧显式绑定 → 虚影消失（关键诊断线索）。

修复：blit 前显式 `bind_framebuffer(READ_FRAMEBUFFER, 视频FBO)` 锁定拷贝源。
附带的状态契约（egui_glow 0.36 源码 painter.rs 确认）：进入 PaintCallback 时
BLEND（premultiplied）/SCISSOR_TEST 已开启；回调返回后 egui_glow 自己重跑
prepare_painting 恢复 viewport/blend/scissor，**唯独 framebuffer 绑定必须由
回调成对恢复（read+draw）**——`intermediate_fbo()` 恒返回 None，文档要求的
恢复目标实为"默认帧缓冲"。

### 坑 3：`smooth_scroll_delta` 惯性尾巴 → 滚轮一次误切两项

它是 egui 的平滑插值量，一次滚轮后连续多帧非零；"检测到滚动即切换 + 按时间节流
（ui.wheel_switch_secs=0.3s）"的方案里，节流窗口一过尾巴残量仍非零 → 第二次
触发切换 → 刚翻到末项就越界误弹「下一个压缩包」询问（点按钮无此问题，因为
click 天然是单次事件）。修复：改用真实 `Event::MouseWheel` 事件驱动（帧内多
事件合并方向判定），节流仅用于防触控板连扫。

### 坑 4：静态 webp 走逐帧解码 → 解出空集误报失败 + 回退后被占位遮挡

现象：静态 webp 一直卡在"正在解码图片…"，mpv 兜底也看不到画面。两层根因：

- image-webp 对静态文件（无 ANMF 块）`num_frames()==0`，`into_frames()` 迭代器
  首查 `current == num_frames()` 直接 None → 解出空集 → 误报"动画无有效帧"回退
  mpv（LOG2：`图片解码失败，回退 mpv 播放: 动画无有效帧`）。修法：webp 全按
  静图走单帧 `decode()`，仅 GIF 走逐帧（回归测试 `decode_静态webp归静态`）；
- 回退只 load mpv 不够：`tex_image_key()` 仍按扩展名返回 Some，panels 的 mpv
  分支要求 `img_key.is_none()` 才渲染 → mpv 已解出首帧
  （`first video frame after restart shown`）却被"正在解码"占位永久遮挡。修法：
  `img_fallback` 集合（见 ARCHITECTURE §13.4 回退链路修复）。

教训：只对 GIF 做过内存编码的单测（image crate 能编 GIF），webp 分支零覆盖——
引入新格式的特殊路径前，先用 `cargo tree`/源码确认该格式解码器的实际行为。

后记（后续演进）：webp 后改为 `has_animation()`（VP8X ANIM 位）先验分流——
动画 webp 走 gif 同款逐帧、静态仍单帧；本坑根因（静态 `num_frames()==0`
空集）正是分流不可省的前提（ARCHITECTURE §13.4，回归测试
`decode_动画webp逐帧`，夹具经 ffmpeg libwebp 生成——image-webp 0.2.4
encoder 不支持动画编码）。

### 坑 5：LRU 只保护刚推入的 key → 滚轮连翻把正在显示的图挤出 → 永久占位

现象：滚轮来回切换几次后回到某 jpg，画面永久停在"正在解码图片…"，且只有
滚轮触发（快捷键/列表点击不会）。根因两层叠加：

- `cache_push` 逐出只跳过「本次刚推入的 key」，不保护「正在显示的 key」。
  滚轮连翻 = 多跳 × 每跳 ±1 预取的推入洪水，cap=3 时正在显示的条目被从
  队尾挤出 → `frame()`=None 走占位分支，而 pending 已空（就绪即推、推完即被
  挤）→ 无人再触发解码 → 死局。快捷键/列表点击是离散单跳只推 2 张，显示中
  条目在队首幸免——精确解释"只有滚轮触发"；
- 修法：`ImageService.shown: Option<ImgKey>`，`frame()` 命中即标记，
  `cache_push` 逐出跳过 pushed + shown，二者都不堪逐时允许临时超容（break，
  下次推入再收缩）。回归测试 `lru_预取洪水不逐出正在显示的条目`。

### 坑 6：占位符分支早退跳过全部输入处理 → 异常态滚轮失灵

`image_view` 原结构 `let Some((tex, _)) = frame() else { 占位; return; }`
把滚轮/缩放处理放在早退之后——一旦卡在占位（正是坑 5 的现场），滚轮翻页
失灵，只能靠文件列表救回。修法：滚轮处理前移到 `frame()` 之前（放大态
缩放分支直接用 `img_view.zoom.unwrap()`，不依赖由 tex 推导的 fit）。教训：
媒体区的输入处理不应依赖"当前帧有画面"这一前提，占位/加载态同样要能翻页离开。

### 坑 7：debug 构建解码慢 10~30 倍 → 误判为 webp 解码器性能问题

现象：打开图片短暂显示"正在解码图片…"，LOG 显示 Jpeg 也要 256ms、WebP 508ms
（release 应为几十 ms 内）。根因：一直在跑 `cargo run`（debug），image/解
码依赖全部无优化。修法：`[profile.dev.package."*"] opt-level = 2`——依赖走
O2、自身 UI 代码保持可调试；另在 worker 发送后 `ctx.request_repaint()` 唤醒
UI 立即 poll（否则空闲时解码完成要等下一次重绘，实测多等 50~130ms）。

### 坑 8：自绘 mesh 无物理像素对齐 → 100% 仍发糊（真根因，2026-10-29 修复）

100% 纹素 1:1 只是"尺寸"1:1，矩形**原点**落在分数物理坐标（逻辑坐标 ×ppp
常见 .5）时，双线性仍会把整幅图混糊；尺寸 f32 误差（3392/1.5×1.5=3391.9995）
还致 LOD 微偏混 mip1。egui 只对文字自动对齐。修复：绘制矩形在物理空间
round 到整像素再换回逻辑坐标（panels.rs 换画段 `(raw.min×ppp).round()/ppp`）。
注意 **DPI 感知无需 manifest**——winit 0.30 默认调
SetProcessDpiAwarenessContext(PerMonitorV2)，ppp 能读到 1.5 即证明进程已感知
（不感知会被虚拟化成 96 DPI、ppp=1.0、全局字体同步糊），勿走"声明 DPI
敏感"弯路。

### 坑 9（流程）：IDE 旧缓冲覆盖磁盘编辑

编辑器开着源文件时保存旧缓冲会静默覆盖 AI 已写入磁盘的编辑（本项目实际发生
多次，表现为编译报"缺字段"）。AI 协作改代码前先 reload 编辑器 / 关掉相关文件。

**附：egui 0.36 API 备忘**——无 `allocate_ui_at_rect`，「视频矩形 + 底部进度条」
用 `allocate_rect` 顺序布局实现。

---

## 3. 播放流畅性专项：mpv core 锁三轮攻坚（player/mod.rs）

> 三轮修复共用同一个根因模型（§3.0），症状不同只是 core 忙窗口出现的时机不同：
> 打开（探测链）→ 切换（属性轮询）→ seek（执行期）。架构描述见 ARCHITECTURE §13.2。

### 3.0 mpv 属性访问的成本模型（一切卡顿的根因框架）

- **命令是异步的**：`command_string` 只把命令塞进 mpv 内部队列即返回（实测 <5ms），
  实际工作（seek 换算、解码、换文件）在 core playloop 后台线程执行；
- **属性读取是同步的**：`get_property` 是 FFI 调用，要拿 mpv core 锁才能读；
  core 正忙（demuxer 探测 / hwdec 试探 / seek 执行）时**阻塞到 core 忙完为止**；
- **推论**：UI 帧路径上任何 `get_property`，都会在 core 忙窗口把整帧 UI 卡住——
  哪怕读的只是 `eof-reached` 这种"便宜"标志。

**修复总纲**：帧路径零 FFI 属性读取——全部状态经 `observe_property` 订阅推送
进原子缓存，UI 帧内纯原子读；`get_property` 仅保留在测试与非帧路径。

附：mpv 属性 / 行为备忘（早期踩过、e2e 均有回归钉死）：

- `pause` / `mute` / `eof-reached` 等 bool 属性必须以 `MPV_FORMAT_FLAG` 读——
  用 INT64 格式读**恒失败**（返回错误码，不做类型转换）；
- `keep-open=always` 在 EOF 时置 `pause=yes` 且 **loadfile 不清除**——切换视频前
  必须显式恢复 `pause=no`，否则新视频停在第一帧（e2e「EOF 后切换视频续播」
  用例钉死此行为）。

### 3.1 第一轮：打开瞬间 UI 冻结 475ms → opening() 门控

- **现象**：loadfile 后 UI 冻结近半秒才出装载动画。
- **根因**：loadfile → 首帧期间 mpv core 被 demux 探测 + hwdec 设备逐个试探
  （d3d11va→nvdec→copy）长段占锁，UI 每帧 5~6 次 `get_property` FFI
  （paused/buffering/duration/time_pos/muted）在 core 锁上排队。
- **修复**：`opening()` 门控——纯原子判定 `load_mark.is_some() &&
  !painted_since_load`（2s 兜底自限），窗口期内上述属性全部短路返回占位值
  不进 mpv。装载动画维持 vsync 节拍，与旧 wid 子窗口模式"黑一下"体验等价。
- **演进注记**：第三轮订阅化后，帧路径已无 get_property，门控的防冻结职责
  消失，但**正确性职责保留**——装载窗口期读到的是上一文件残留在原子缓存里
  的旧值（新文件的属性事件尚未到达），必须短路返回占位值。

### 3.2 第二轮：视频切换小卡顿 → 常驻事件泵 + 命令化写操作

- **现象**：连续切视频/调音量/暂停时偶发小顿挫。
- **修复**（四件套）：
  1. **常驻事件泵**：事件泵线程 ctor 即启动、`wait_event(0.2)` 常驻阻塞
     （原先每帧 `wait_event(0)` 轮询，忙等且吃事件延迟）；
  2. **mute/pause 订阅化**：第一批 observe_property（FLAG → AtomicBool），
     UI 读态不再 FFI；
  3. **写操作命令化**：音量/暂停等改发 `cycle`/`set` 命令（异步入队），
     不再 `set_property`（同步拿锁）；
  4. **hwdec per-file 智能记忆**：首文件探测 hwdec 结果后记忆，后续文件
     直接设定跳过逐设备试探（试探是打开期 core 长段占锁的主力之一）。

### 3.3 第三轮：进度条 seek 小卡顿 → 四属性订阅化（2026-10-30）

**现象**：拖动进度条松手 / 点击进度条跳转后，UI 有一次 ~200ms 小冻结。

**实证归因**（探针数据）：

| 证据 | 读数 | 结论 |
|---|---|---|
| seek 探针（≥5ms 阈值） | **零输出** | seek 命令本身 <5ms，排除命令路径（成本模型：命令异步） |
| 帧探针（16 分段） | `eof_tick` 段 **194.3ms / 193.8ms** 两次 | 恰好对应两次 seek 操作（点击 + 拖动），卡点锁定在该段 |
| 代码审查 | `video_eof_tick` 段内**唯一** mpv 调用 = `eof_reached()` getter | 帧内顺序 eof_tick→顶部条→控制条，首读吸收全部阻塞 |

**根因**：seek 命令异步入队后，实际 seek 执行（demuxer 时间→字节换算、
zip VFS 重解丢弃）在 core 后台**持锁**进行；seek 后没有 opening() 保护窗口，
下一帧 UI 的 `eof_reached()`（get_property）在 core 锁上排队等 ~194ms →
整帧冻结。正是成本模型推论的标准形态。

**修复**（六属性全订阅缓存，含第二轮两项）：

| 属性 | mpv 格式 | 原子缓存 | 变更源 |
|---|---|---|---|
| `mute` / `pause` | FLAG | `AtomicBool` | 用户操作 + mpv 内部 |
| `eof-reached` / `pause-for-cache` | FLAG | `AtomicBool` | mpv 播放推进（EOF / 缓冲） |
| `time-pos` / `duration` | DOUBLE | `AtomicU64`（f64 位模式 `to_bits`/`from_bits`） | mpv 每帧 / 装载完成 |

- **泵分发**：事件泵 `MPV_EVENT_PROPERTY_CHANGE` 按属性格式分发——FLAG 读
  `*const c_int`、DOUBLE 读 `*const f64`（data 指针按格式重解释，双格式分发
  是本轮泵改造的核心）；
- **getter**：全部改为纯原子读（无 FFI、无锁、纳秒级）；`opening()` 门控
  保留（正确性职责，见 3.1 演进注记）；
- **mpv_ffi 侧**：新增 `observe_property_fmt` / `observe_property_double` helper；
  `get_property_flag` / `get_property_f64` 从生产路径退役，转 `#[cfg(test)]`
  供测试消费。

**订阅洪流量化**（为什么订阅不会反过来淹死泵）：

- 生产上限 ≈ 帧率：`time-pos` 每视频帧一条 ≈ 60 条/s（最高频），其余属性
  只在状态翻转时各一条；
- 泵消费成本百纳秒级/条（写原子变量 + debug 日志判断），60 条/s 完全无感；
- UI 卡顿时 mpv 内部队列溢出自动**丢最旧**事件——订阅是"最新值快照"语义，
  丢中间值无害，恢复后下一条即对齐。

**扩展套路**（以后任何属性想进帧路径，三步）：

1. App/Player 加 `AtomicBool` / `AtomicU64` 缓存字段 + 纯原子读 getter（含
   opening() 短路）；
2. 构造时 `observe_property(名字, 格式)`；
3. 事件泵 `PROPERTY_CHANGE` 分发 match 加一行。

**测试**（mpv_ffi.rs `ffi_observe_property` 四段）：符号加载 / 环境跳过 /
基础订阅回调 / **实播验证**——ffmpeg 生成 3s mp4 真播放，断言 `time-pos`
DOUBLE 事件持续到达且数值单调递增，钉死 `MPV_FORMAT_DOUBLE = 5`（手写 FFI
枚举值无编译器保护，坑 1 同款风险，用实播钉死）。

**复测解读**（修复后探针残余，均为非缺陷）：

- EOF 后 ~100ms 慢节拍：mpv keep-open 下 eof-reached 事件本身延迟上报，
  非卡顿（帧探针无长段）；
- 首文件偶发长帧：hwdec 首探测 + 首帧 GLSL 编译，进程一次（见 §3.4 cadence
  基线注记）；
- debug 构建抖动：坑 7 教训，性能结论以 release 为准。

**教训**：症状在 seek，病根在帧路径的 get_property——seek 只是制造 core 忙
窗口的诸多时机之一。若只对 seek 做特殊处理（如 seek 后短暂门控），下次
core 忙的时机（换文件、缓冲、字幕轨切换）还会复发；按成本模型把属性读取
整体赶出帧路径才是一劳永逸。

### 3.4 常驻探针体系与日志级别策略

三轮攻坚的定位全部依赖常驻探针（debug 级静默，RUST_LOG 打开即出）：

| 探针 | 位置 | 触发阈值 | 输出 |
|---|---|---|---|
| 帧探针 | ui/mod.rs `frame()` | 单段 ≥30ms 或帧总 ≥50ms | 16 分段耗时表（布局/输入/绘制拆段） |
| seek 探针 | player/mod.rs `seek()` | ≥5ms | 命令入队耗时（异步性的直接证据） |
| cadence 探针 | panels.rs `Cadence` thread_local | 常驻汇总 | 每 5s：paints/avg/max/长帧归因（render 侧 vs ui 侧）/mpv 换帧数/render 最重耗时 |

基线（release，60Hz/30fps）：avg=16.7ms 恒定、render 最重 <1ms、ui 侧长帧
~2 个/5s（OS 底噪）、首窗 render 侧 1~2 个 70~90ms（首帧 GLSL 编译，进程一次）。

**日志级别策略（定稿）**：

- **debug**：诊断/探针/时序类（帧探针、seek 探针、mpv 日志行、事件时间线、
  日志泵启动、首帧渲染、hwdec 探测记忆）——本轮 8 处 info→debug 降级
  （player/mod.rs 6 处 + ui/mod.rs 帧探针 3 处），release 默认静默；
- **info**：用户动作与业务生命周期（后台解压、缓存命中、会话重定向等 §4
  手册所列日志——它们是排查手册的 grep 关键词，必须保持 info）；
- 优先级：`RUST_LOG` 环境变量 > config.toml `log_level` > 默认 info（main.rs 启动序）。

---

## 4. 点击状态机排查手册（start_extract 分支链与日志对照）

> 机制设计（会话化协议、滑动窗口、keep 分支判定）见 ARCHITECTURE §11.4；
> 本手册回答"用户每次点击后发生了什么、各自打什么日志"。排查「点了没反应 /
> 后台解压停了 / 画面跳来跳去 / 窗口被拽回去」类问题时，以本节日志关键词
> grep 日志流即可对号入座。

**两个正交变量**（所有分支日志都同时打印它们，混淆二者是多数误判的根源）：

- `playing`（`App::playing`）——**用户正在看什么**：UI 列表高亮与滚动跟随的唯一依据，
  由 `load_extracted` 在播放时写入；
- `focus`（`session.focus`）——**解码线程在解什么**：抢道锚点 = 上次重定向写入的目标
  下标，**只向下游推进不回退**（keep 分支不改写），仅解码线程消费。等待 80 解压时回看 10：
  `playing=10` 而 `focus=80`，两者不同才是正常态。

**分支判定链**（ui.rs `start_extract`，按代码顺序短路，命中即返回）：

```
点击 entry
 ├─ ① j.entry_idx == entry？ ─────────────── 是 → 静默返回【去重早退，无日志】
 ├─ ② cached && 会话存活 && 点击在锚点上游 ── 是 → keep【回看，不碰会话】
 ├─ ③ 窗口重算为空（整窗已落地）？ ─────────── 是 → 仅播放【不重定向、不动 focus】
 ├─ ④ 磁盘放不下新窗口？
 │    ├─ cached ──→ 降级：播放不受影响，沿用旧窗口不滑动
 │    └─ 未缓存 ──→ 报错返回，不起解压
 └─ ⑤ 重定向（同锁：targets ← 新窗口、focus = entry、gen += 1）
       ├─ 会话存活（解码线程在跑，流续跑，cancel/通道复用）
       │    ├─ cached ─────→ 缓存命中（窗口向下游滑动，立即播放）
       │    ├─ was_pending → 批内切换目标
       │    └─ 其他 ───────→ 批外会话重定向
       └─ 会话已死 → spawn 新线程（新 cancel/通道）
             ├─ cached ──→ 缓存命中（预取会话启动）
             └─ 未缓存 ─→ 后台解压
```

判定依据：`cached` = 目标文件在盘且尺寸一致；`was_pending` = 目标在当前任务 pending 表；
「锚点上游」按 **view 序**比较（`pos_of` 把点击项与 focus 都转 view 位再比，sort 开启时也
正确；任一端不在当前 view——被过滤——则判否，**退回重定向路径**，安全默认）。

**日志对照表**（文案与代码逐字一致，grep 左列关键词即命中）：

| 分支 | 日志开头（关键词） | 级别 | 附带状态字段 |
|---|---|---|---|
| ① 去重早退 | （无日志） | — | 连点同项时无任何输出 |
| ② keep | `缓存命中（回看，会话保持）` | info | 点击 view 位 / 锚点 focus（view 位）/ 切换前 playing / 后台待落地 N 项 / 窗口（当前解码会话窗口摘要） |
| ③ 整窗已落地 | `缓存命中（整窗已落地）` | info | 锚点 focus / playing / 当前窗口 |
| ④a 磁盘不足降级 | `窗口滑动跳过（磁盘不足，播放不受影响）` | warn | 锚点 focus / playing |
| ④b 磁盘不足报错 | `缓存目录所在磁盘空间不足` | error | 剩余/所需 GB、缓存目录路径 |
| ⑤a 缓存命中滑窗 | `缓存命中:` | info | 锚点 focus 旧→新 / 切换前 playing / 窗口摘要 |
| ⑤b 批内切换 | `批内切换目标:` | info | 窗口滑动至 N 项（物理 a..=b）/ 锚点 focus 旧→新 / 切换前 playing |
| ⑤c 批外重定向 | `会话重定向:` | info | 新批 N 项（物理 a..=b），解码流续跑 / 锚点 focus 旧→新 / 切换前 playing |
| ⑤d 预取会话启动 | `缓存命中（预取会话启动）` | info | 锚点 focus 旧→N / 切换前 playing / 新窗 N 项（物理 a..=b） |
| ⑤e 后台解压 | `后台解压:` | info | 目标路径 / 顺带预取 N 项 / 锚点 focus 旧→N / 切换前 playing / 窗口摘要 |
| — 会话死亡 | `会话解净空闲退出（窗口清空）` | info | 最后目标 / 锚点 focus / 当前窗口（空）/ playing；此后 keep 失效，缓存命中点击转 ⑤d |

> 字段说明：`切换前 playing` = 本次点击切换**之前** `App::playing` 的值（时间维度的"前"，
> 与上/下游方位无关）——用于对照切换后画面是否如期变化。`锚点 focus 旧→新` 的"旧"是
> 本次重定向写入前的 focus（可能是 None）；`窗口摘要` = `N 项，物理 a..=b`（targets 的
> 物理下标范围 + 条目数；keep/③ 打印的是**当前**会话窗口，⑤a-⑤e 打印的是**新算出的**
> 窗口）——用来一眼看出"点击项在不在后台正在解的范围内"（LOG2 的来回重定向问题：
> 批内切换 105 项 ↔ 会话重定向 106 项恒定不变 = 没有任何文件落地）。
>
> **③ 整窗已落地 vs 普通缓存命中（②/⑤a/⑤d）的区别**——都以"点击项已在盘"为前提，
> 差在**重算出的窗口里还有没有未落地条目**：
>
> - 普通缓存命中：点击项已落地，但窗口（目标 + 下游预取 + 上游免费区）内仍有未落地
>   条目，后台还有活——②keep 完全不碰会话（点击项在锚点上游）；⑤a 窗口向下游滑动
>   后重定向（点击项在锚点下游/同位）；⑤d 会话已死（上轮解净空闲退出过）则重新拉起
>   预取会话，围绕已播目标继续解后续；
> - ③ 整窗已落地：以点击项为中心重算的**整个窗口全部已在盘** → targets 为空，后台
>   无事可解——此时**不能**重定向（空 targets 会让解码线程判空退出，若线程正跑等于
>   误杀），也不动 focus（锚点不因全缓存回退），仅播放 + 切 job 字段。
>
>   注意 ② 在 ③ 之前：缓存命中 + 会话存活 + 点击在锚点上游的组合已被 keep 拦截，
>   ③ 只在 ② 判否**且**窗口重算为空时才可达。② 判否的原因即 ③ 的真实触发场景：
>   （a）**会话已死**（上轮解净空闲退出，alive=false）——翻回整个缓存区逐个点
>   全走此分支，最常见（滑窗冒烟测试最后一步"解净退出后点 0"即此）；（b）点击在
>   锚点**同位/下游**且其窗口恰全落地——keep 用严格小于比较，同位不拦；点回锚点
>   本身、点包尾、或 `prefetch_downstream_mb=0` 时点下游缓存项；（c）锚点或点击项
>   被过滤不在 view（keep 判否的边界，罕见）。
>
>   ③ 不起会话**不会**造成"点完缓存才想起解压"的空窗：③ 后继续向下游点缓存项，
>   只要新窗口（含下游预取预算）内有未落地条目就走 ⑤d 立即拉会话、预取续跑——
>   顺序观看时预取始终跑在人手前面；只有跳到预算外（>512MB 下游）的未落地项才
>   现场等待，而这与会话死活无关（会话活着跳同样远也一样等）。

**keep（②）与重定向（⑤）的本质差异**——keep 是唯一不碰会话的播放路径：

| 动作 | ② keep | ⑤ 重定向 |
|---|---|---|
| `session.targets`（窗口） | 不动 | clear + 写入新窗口 |
| `session.focus`（锚点） | 不动（只向下游推进） | = 点击项（向下游推进） |
| `session.gen` / `App::next_gen` | 不动 | 各 +1 |
| `j.pending`（"剩余 N 项"数据源） | 不动（后台量如实） | 重建 |
| `j.entry_idx`（Done 兜底依据） | = 点击项 | = 点击项 |
| cancel 句柄 / 解码线程 | 不动 | 存活则复用，已死则重 spawn |

**落地侧**（`poll_extract` 每帧消费线程消息，注意两条**无日志**路径）：

- `Progress(stage, done)`：浮层文案/进度条数据源，**覆盖语义**（后到整体替换先到）。
  **坑（消息积压幻象）**：若图省事每帧只消费一条 Progress 就 `return`（10fps ≈ 10 条/秒），
  而线程每落地一个文件发 ≥1 条（copy_with_progress 每 4MB 一报 + 小文件首尾各报，
  879 项批 ≈ 两千多条），通道积压分钟级，三大症状连锁：① 线程早已判空退出
  （`alive=false`），浮层仍转圈"正在解压/跳过前置文件"——**积压驱动的过期显示，
  极易据此误判会话存活**；② `Done` 排在积压尾，`会话解净空闲退出` 死亡日志迟迟
  不打；③ 下一次点击走 ⑤d spawn 时整体替换 `job.rx`，整个积压连同 `Done` 被丢弃——
  死亡日志**永远**不出现，于是"回看已缓存项却打出预取会话启动"变得不可解释
  （实为会话几分钟前已死，见下 ⑤d 与 Done 条目）。修复：排空循环——Progress 只留
  最新一条（覆盖语义下中间态无意义），FileDone/Done/Failed 即时逐条处理。

- `FileDone(i)`：从 pending 移除；`i == j.entry_idx && playing != Some(i)` → 立即自动开播
  （**无日志**，判断依据 = 画面切换 + 日志里"后台待落地 N 项"递减）；预取项落地只记账。
  keep 之后 `j.entry_idx` 是回看项，80+ 的落地不会把画面拽走——这正是 keep 的安全性来源；
- `Done`（整批解净，会话空闲退出）：`extract = None`，info 日志
  `会话解净空闲退出（窗口清空）`（锚点/窗口/playing 快照）——**会话在此死亡**，
  此后 keep（要求会话存活）失效，下一次缓存命中点击走 ⑤d 重新拉起预取会话并
  重写锚点（LOG1"回看中突然预取会话启动"即此转折，此前无日志不可见）；
  兜底补播仅当 `gen == next_gen && playing != Some(entry_idx) && 文件在`
  （keep 后 playing 已是点击项 → 不触发，画面稳定）；
- `Failed`：error 日志 `解压失败: 压缩包 [..] 条目 [..] → [..]: {e}` + 弹错误框；密码
  错误（`VfsError::Password`）**不落日志**，直接弹密码框（提交后重建 VFS 重试）。

**解码线程侧**（仅两条，见 ARCHITECTURE §11.4）：

- info `7z 会话本轮零落地但检测到重定向（gen X→Y），重新规划续跑`——gen 救回：重定向
  打断后本轮剩余流零落地属正常，重规划而非误报。**看到它说明会话健康，不是错误**；
- `7z 会话无进展: 目标条目未出现在 folder 解码流中`——真无进展（gen 未变的零落地轮），
  上浮为 error `解压失败: ...`，会话已置 `alive=false`，下次点击走 ⑤ spawn 路径。

**变量速查**（日志里出现时的读法）：

| 字段 | 含义 | 写者 → 读者 |
|---|---|---|
| `playing` | 正在播放的条目（UI 高亮/滚动跟随） | `load_extracted` → 渲染/兜底判定 |
| `锚点 focus` / `锚点 focus→N` | 后者仅出现在重定向日志 = 锚点**推进**到 N | UI 重定向 → 解码线程（folder 抢道/漂移） |
| `gen`（session）/ `next_gen`、`j.gen`（UI） | 重定向代数；`j.gen == next_gen` 表示期望未被后续点击取代 | UI 同锁自增 → 线程零落地救回 / Done 兜底 |
| `后台待落地 N 项` | `j.pending.len()`，即"剩余 N 项"顶栏数据 | FileDone 递减 → 顶栏显示 |
| `窗口 N 项，物理 a..=b` / `窗口摘要` | 会话 targets（解码线程正在解的工作集）条目数 + 物理下标范围；keep/③ = 当前窗口，⑤a-⑤e = 新算窗口 | `compute_batch` 组批 → 日志观测"点击项是否在解"（窗口数恒定不变 = 零落地） |
| `点击 view 位 / 锚点 view 位` | 双方的 view 序位置（sort 开启时 ≠ 物理下标） | keep 判定的直接依据（前者 < 后者 = 点击项在锚点上游） |

**典型时序回放**（用户场景：看过 1-30 已缓存，跳 80 等解压，回点 10、11，再点 81）：

```
后台解压: clip_80.mp4 → D:\cache\..（顺带预取 15 项）｜锚点 focus 29→80，切换前 playing=Some(29)，窗口 16 项，物理 80..=95
缓存命中（回看，会话保持）: clip_10.mp4｜点击 view 位 9，锚点 focus=80（view 位 79），切换前 playing=Some(80)，后台待落地 15 项，窗口 16 项，物理 80..=95
缓存命中（回看，会话保持）: clip_11.mp4｜点击 view 位 10，锚点 focus=80（view 位 79），切换前 playing=Some(10)，后台待落地 14 项，窗口 16 项，物理 80..=95
批内切换目标: clip_81.mp4（窗口滑动至 16 项，物理 81..=95）｜锚点 focus 80→81，切换前 playing=Some(10)
7z 会话本轮零落地但检测到重定向（gen 1→2），重新规划续跑   ← 上一步重定向打断旧轮，救回属正常
（此后后台健康解压 = 顶栏剩余数递减、窗口摘要里的落地项逐步出窗；整批解净多一条
"会话解净空闲退出（窗口清空）"；画面停在 81 落地自动开播——无日志）
```

**常见症状速查**：

| 症状 | 先查 |
|---|---|
| 点了没反应 | ① 去重早退（无日志，`j.entry_idx` 已是它）；或条目被过滤（不在 view → `start == usize::MAX`，目标单独成批）；或 `prefetch_downstream_mb=0` 且目标已缓存（窗口为空走 ③，查 `缓存命中（整窗已落地）`） |
| 后台 80+ 解压停了 | 最近一条点击日志是 `会话重定向`/`批内切换` 且锚点回退上游？→ keep 判定失效（点击项或锚点不在 view，被过滤条目退回重定向）。正常应见 `缓存命中（回看，会话保持）` |
| 画面被拽走/跳来跳去 | FileDone 自动开播条件 `i == j.entry_idx && playing != Some(i)`——keep 后 j.entry_idx 是回看项不该触发；若触发说明走了重定向（查 `锚点 focus→N` 日志） |
| 顶栏剩余数跳变后冻结 | 剩余数 = `j.pending.len()`，keep 不动它（如实）、重定向重建它；冻结 + 无 `解压失败` → 线程可能真无进展（查 `7z 会话无进展` 上浮的 error） |
| 反复出现 `重新规划续跑` | 每次重定向打断一轮都可能救回一次，正常；但若**没有**任何点击日志却持续出现，需查 UI 侧是否有意外调用 `start_extract` |
| 来回点两个未落地条目疯狂 `会话重定向`/`批内切换` | 窗口摘要恒定不变（=零落地）；两点击项互不在对方窗口（上游侧未覆盖：`prefetch_upstream_mb=0` / 7z folder 边界 / sort 后物理距离超预算）——重定向打断解码使两边永远落地不了，见"会话死亡"前的窗口对比 |

---

## 5. 解压会话化回归测试细节（ARCHITECTURE §11.4 的测试断言明细）

### 5.1 解码线程侧单测（sevenz_vfs.rs，4 个）

- `session_lifecycle_and_reuse`：空闲退出 / 空目标立即返回 / 复用时 focus 抢道
  落地顺序 [3,0] / 脏目标 Err(Archive)；
- `session_dynamic_targets_chain`：回调链式追加 → 单次调用解出全部四个；
- `session_focus_drift_interrupts_folder`：solid folder 内切 focus → 漂移中断 →
  自愈补齐，落地顺序 [g1, h1, g3] 且三文件内容全验证；
- `session_zero_landing_rescued_by_gen`：65 条目 solid 包中途重定向 → 本轮零落地 →
  gen 0→1 救回重新规划解出新目标而非误报 Err（旧代码此处直接报错中止会话）。

### 5.2 UI 层冒烟测试（ui.rs `sliding_window_smoke_in_batch_back_and_forth`）

模拟用户批内来回跳动：不走 egui 渲染，直接驱动 `start_extract` + 轮询
`poll_extract` 模拟 UI 主循环（测试构造器 `new_for_test` 绕开
`eframe::CreationContext`；`start_extract`/`poll_extract` 不依赖 frame 参数，
天然可测）。6 × 1MB 伪随机 jpg 压成单 folder solid 7z，下游预取 3MB
（`prefetch_downstream_mb=3`）使窗口恰为 [目标, +1, +2]、上游预取关闭
（`prefetch_upstream_mb=0`）让断言值可硬编码；跳转链
0→1→0→2→1→0→（解净空闲退出）→5→0 覆盖全部分支：

- 批内切换以 `Arc::ptr_eq(cancel)` 断言解码线程**不重启**且 `started` 不清零
  （覆盖层宽限期不清零）；
- 回跳已落地目标（点击项在锚点上游）走 keep——立即播放且**会话纹丝不动**
  （窗口/focus/gen/pending 快照对比全等、cancel 同句柄），连点多个缓存项
  （点 1 再点 0）仍全 keep（复现"点完 10 再点 11"场景：绝不触发窗口重建）；
- 向下游点击未落地项才重定向滑窗 + 锚点向下游推进 + gen 自增；
- 解净后画面停在回看项（keep 的 Done 兜底不触发）；空闲退出后再跳重 spawn
  （新 cancel 句柄）；整窗已落地不重定向（空 targets 防误杀空闲线程）；
- 终态 6 文件逐字节与源数据比对（防落地错位）+ 会话干净退出。1MB 文件解码为
  百 ms 级而断言紧跟落地事件（μs 级），窗口期约千倍差，时序不抖。

---

## 6. 测试运行方式与覆盖矩阵

> 从 README 迁入（用户向文档只保留用法与配置）。会话化专项断言明细见本文 §5。

### 6.1 运行方式

```bat
run_tests.bat
```

或直接：

```bat
cargo test -- --nocapture
```

### 6.2 覆盖矩阵

| 测试 | 验证内容 |
|---|---|
| VFS 单元测试 | ZIP / 7Z / RAR 各自的列表、读取、seek 正确性；7Z 测试包由 sevenz-rust 纯代码构造，无外部依赖 |
| DirVFS 单元测试 | 目录媒体集合：递归收集与字典序确定性（跳过压缩包/非媒体）、条目读取与 seek 正确性、未知条目报错与空目录空集合、根不可读报错 |
| 目录模式集成（UI 层） | spawn_open 目录分支真实线程链路（DirVFS::open → stream_cb 注册 → Done 消息）、打开终态状态迁移（FastSeek→stream 标志/视图重建）与空目录统一提示 |
| e2e 流式播放 | headless mpv（vo=null ao=null cache=no）经 zvf:// 协议真实播放 deflate ZIP 内的 mp4，验证下游 / 上游 seek，以及 EOF 后切换视频能正常续播（keep-open 暂停粘滞回归，见 §3.0 备忘） |
| e2e 解压播放 | 构造 7Z 包 → 后台解压 → headless mpv 播放本地文件全链路 |
| flag 属性循环 | pause / mute 属性经 flag 格式读 → 翻转 → 写 → 读，覆盖空格暂停、静音按钮的行为路径（FLAG 格式约定见 §3.0 备忘） |
| VarTidy | 分类规则（五类非法+合法路径）、仓库只读三道闸（含 rename 不覆盖）、两级去重（CRC 判重）、密码候选链路（AES zip / 7z 加密头与内容加密 / 同批次多包各不同密码乱序命中 / 中文与特殊字符密码）、中文/空格/特殊字符作者名与包名全链路 |
| 滑动窗口冒烟（UI 层） | 模拟用户批内来回跳动 0→1→0→2→（解净空闲）→5→0：断言解码线程全程不重启（cancel 句柄同一性）、预取窗口随观看位置滑动、缓存命中立即播放、全部落地文件逐字节内容校验（明细见 §5.2） |
| config | TOML 默认模板与代码默认值防漂移、序列化往返、特殊字符密码（`#`/空格/中文）引号保留 |

### 6.3 headless e2e 原理与外部依赖

e2e 测试不需要人工看画面：mpv 以无渲染模式运行，通过 `time-pos` / `pause` /
`eof-reached` 等属性断言播放状态，`cache=no` 强制 seek 请求打到 VFS 层，能真实
暴露流实现缺陷。

外部依赖两条：

- **RAR 测试**需要本机装有 WinRAR（探测环境变量 `ARCHVIEW_TEST_RAR` 或
  Program Files 常见路径，找不到自动跳过，不算失败）；
- **VarTidy 本地冒烟环境**：`test_tidy\setup.bat`（目录已 gitignore）一键全链路
  ——构建 → `cargo run --example make_smoke_fixtures` 生成 6 个测试包
  （明文/AES zip/明文/内容加密 7z/特殊字符密码 + 既有仓库）→ dry-run →
  实际整理（加密包打开期即锁定密码）→ 幂等重跑。
