# §13 空闲 CPU 全事件驱动专项

> 对应架构章节：[ARCHITECTURE §13.2 帧呈现节奏](../架构设计/13_渲染架构迁移.md)。
> 本篇只记现象、根因、修复过程与验证方法；机制设计见架构章节。

## 现象（用户实测）

1. 软件刚启动、**未打开任何媒体**，CPU 占用 ~2%（任务管理器）；
2. 单图模式静置占用与空闲态几乎相同；
3. **多图对比模式**静置占用 ≈ 3× 单图模式；
4. 期望：空闲 / 静态图静置 / 对比静置 / 视频暂停，CPU 趋近 0。

## 根因

[ui/mod.rs](../../src/ui/mod.rs) `App::ui` 帧首的**全局心跳**：

```rust
if video_live { ctx.request_repaint(); } else { request_repaint_after(100ms); }
```

只要不在"视频播放中"（video_live）状态——含**完全空闲**、图片模式、对比模式、
**暂停**——每帧无条件预订 100ms 后重绘。egui 立即模式 UI 的每次重绘 = 全量
重建整棵 UI（列表、布局、tessellation、纹理绑定、present），画面一个像素没变
也照跑。10 FPS 全量重绘 ≈ 2% CPU，这就是"空闲态基线"。

对比模式 3× 的来源：每帧成本 × 栏数——每栏 `allocate_rect` 交互控件、
mesh 构建与物理像素 snap、名称徽标文字排版、`ImageService::frame_ex` 的
LRU 重排，外加 `compare_tick` 对每个非流式栏每帧做 `std::fs::metadata`
落地检查（3 处调用点，纯磁盘 syscall 轮询）。

### 为什么"必须心跳"其实是伪命题（审计结论）

逐个审计 `ui()` 里每帧调用的 tick/poll 后发现，除 4 类外全部不依赖心跳：

| 机制 | 原唤醒方式 | 是否真需要心跳 |
|---|---|---|
| ImageService 解码（imgview.rs） | 解码线程完成时主动 `request_repaint` | 否 |
| RawFrameService（rawdec.rs） | 同上 | 否 |
| mpv render 帧更新 | render update 回调逐 vsync 唤醒 | 否 |
| 编码任务（dialogs.rs） | 线程 100ms 节流唤醒 + 终态唤醒 | 否 |
| 快捷键/密码框/配置编辑器/对话框 | egui 输入事件自动出帧 | 否 |
| 搜索防抖/解码占位符/动图/徽标闪现 | 各自条件 `request_repaint_after` | 否 |
| **打开任务**（open.rs） | 线程只 send 不唤醒 | **是→改为 send 后唤醒** |
| **解压任务**（extract.rs） | 同上 | **是→改为 send 后唤醒** |
| **EOF 粘滞推进 + mpv 错误回显** | 泵线程只 send 不唤醒 | **是→改为事件唤醒** |
| **对比落地等待** | 每帧 metadata 轮询（无事件） | **是→FileDone 事件 + landed 缓存** |
| **幻灯片** | 心跳轮询到期 | **是→精确排帧** |

## 修复（六项）

1. **全局心跳删除**（[ui/mod.rs](../../src/ui/mod.rs)）：`video_live` 逐 vsync
   保持不变；幻灯片开启时精确 `request_repaint_after(slideshow_rest(now))`；
   **其余状态零排帧**（原 else 100ms 分支整个删除）。
2. **open 线程补唤醒**（[ui/open.rs](../../src/ui/open.rs)）：闭包捕获
   `egui::Context`，每条 OpenMsg send 后 `request_repaint`（含 zip list
   进度回调——request_repaint 是原子置标志，逐条目高频调用开销可忽略，
   egui 自动合并为下一帧）。
3. **extract 线程补唤醒**（[ui/extract.rs](../../src/ui/extract.rs)）：同上，
   Progress / FileDone / Done / Failed 四类消息 send 后唤醒。
4. **mpv 泵线程补唤醒**（[player/mod.rs](../../src/player/mod.rs)）：
   `PLAYBACK_RESTART`（opening 门解除）、`eof-reached` 属性置位（EOF 粘滞
   推进）、`END_FILE(ERROR)` send 后（装载失败横幅回显——mpv 出错后停止
   渲染、update 回调不再触发，没有这条唤醒错误横幅要等到用户动鼠标才显示）。
5. **幻灯片精确排帧**（[ui/nav.rs](../../src/ui/nav.rs) `slideshow_rest`）：
   剩余时长 saturating 计算，每个翻页间隔只醒一次；翻页后 1~2 帧的锚点
   重置延迟由 `slideshow_tick` 的 idx≠playing 重置逻辑自愈，多出的立即帧
   可忽略。
6. **对比落地缓存**（[ui/compare.rs](../../src/ui/compare.rs) +
   extract.rs）：`ComparePane` 新增 `landed: bool`（流式恒 true；非流式
   build 时查一次 metadata，此后由 `poll_extract` 消费 `FileDone{idx}` 时
   置位——`ui()` 帧内 `poll_extract` 先于 `compare_tick`，同帧拾取）。
   `compare_tick` 三处与 `compare_after_change` 一处的每帧 `source_ready`
   （fs::metadata）全部改读缓存；原 `size` 字段失去唯一读者，删除。

## 排查提示

- 表现"画面/状态卡住、动一下鼠标就刷新" = 存在依赖心跳但无唤醒源的路径：
  先 grep `request_repaint` 确认对应后台线程/事件分支是否补了唤醒；
- 日志关键词：帧探针 `[帧探针]`（≥30ms 分段打点）可对照确认空闲时是否
  真的没有帧；`发起解码`/`纹理上传` 对照解码唤醒链路。

## 教训

- egui 按需重绘模型下，`request_repaint_after(固定间隔)` 作全局兜底是
  "轮询思维"的惯性——正确姿势是**每个后台完成事件都有主动唤醒**，UI 只在
  有事发生或显示动态内容（视频/动图/幻灯片）时才醒；
- `std::fs::metadata` 每帧调用在空闲分析里容易漏（不是 CPU 大头但属于
  无谓 syscall），落地判定这类"等待外部状态"应该事件化（此处事件源
  FileDone 本来就存在）。

## 追加：空闲态鼠标悬停风暴（用户实测）

**现象**：什么也不开，鼠标在 UI 区域晃动，CPU/GPU 逐步升到 ~10%（随移动
速度攀升，停下回落）。

**根因**：egui-winit 0.36 对 `CursorMoved` **无条件** `EventResponse { repaint:
true }`（egui-winit lib.rs:352-358，`on_cursor_moved` 无节流），eframe 的
`NativeOptions::max_fps` 仅 web 后端实现，native 无帧率上限——鼠标事件率
（125~1000Hz）直接驱动帧率，空闲态每帧便宜（~1-2ms），帧率可达数百 fps。
"逐步提升"= 事件率随移动速度变化 + 任务管理器平滑。**非本次重构引入**
（重构前悬停同样唤醒帧，只是被 2% 心跳基线掩盖）。

**修复**（[ui/mod.rs](../../src/ui/mod.rs) ui() 帧首）：**非视频播放态所有帧**
统一节流——`frame_start` 记录上帧起始（**补眠后**的实际起始），距上帧不足
16ms 则 `thread::sleep` 补差额，帧率封顶 ~60Hz。不限 PointerMoved 帧：
open/extract 进度唤醒（NVMe 上 4MB 节流回调可达数百次/秒）、egui hover
动画帧同样是风暴源。睡眠合并积压事件；期间到达的后台唤醒延迟 ≤16ms
不可感知。
**视频播放态排除**：其重绘循环已被 vsync swap 封顶在刷新率（"vsync 主钟"
设计），鼠标/事件汇入同一节奏不产生额外帧，节流是 no-op；而帧首睡眠会把帧
起始推过 vsync 线，制造掉帧 judder。
**幻灯片/动图态纳入**（初版曾排除，复盘修正）：翻页与动图帧由 timer 唤醒、
间隔远大于 16ms 不触发睡眠，翻页精度不受影响；仅风暴被节流。
**两处返工记录**：① 初版只节流 PointerMoved 帧+空闲态——线程唤醒帧
（解压进度）漏网，幻灯片态被误排除；② 初版 frame_start 记补眠前时刻，帧
间距漂移成"短+长"交替（平均 ~100fps 而非 60fps）——改记补眠后实际起始。
**观测**：ui() 帧首有 `[帧率自检]` debug 日志（每 5s 一条，RUST_LOG=archview
=debug），验证节流是否生效以此为准——悬停 ~30fps、拖拽 ≤60fps、空闲 0。
**配置化**：三档间隔由 [ui] redraw_hover_fps / redraw_drag_fps /
redraw_other_fps 驱动（默认 30/60/60，pacing_ms 钳制 5~240fps），弱机可调低
redraw_hover_fps 进一步省；高刷屏可调回 60。release 弱机实测（RX 640）：
60fps 悬停 CPU/GPU 各 ~10%/11%（egui 全量重建+present 的固有成本），30fps
档减半——即 redraw_hover_fps 默认 30 的依据。命名以"UI 重绘频率"为心智
模型（redraw_* 而非 fps_*），避免与视频 fps 混淆——初版 fps_hover/drag/
event 因语义不清返工一次。

**教训**：egui "事件驱动重绘"没有事件侧节流概念，事件率 = 帧率上限；需要
帧率上限时只能在 App 层补（睡眠 pacing 或 patch egui-winit），睡眠期间
winit 事件队列天然合并批处理，是免 patch 的最小实现。

## 验证

- `cargo test`：176 项全绿（改动不涉纯函数逻辑，compare 结构体加字段）。
- 手动矩阵（`cargo run --release` + 任务管理器）：
  - 启动即空闲：2% → ~0；鼠标悬停/点击瞬时出帧；
  - 空闲态快速晃动鼠标：CPU/GPU 不超过 ~1-2%（帧率封顶 60Hz），随移动
    速度变化的"逐步提升"消失；
  - 静态图静置 ~0（动图按帧延迟轻量）；对比模式落地完成后静置 ~0；
  - 视频暂停静置 ~0；恢复播放、List 模式 EOF 自动下一个、装载失败横幅正常；
  - 打开大包/解压在途进度条刷新正常，完成后静默；幻灯片按间隔翻页正常；
  - 搜索防抖、解码占位符"已等待"计时、徽标闪现、动图播放不变。
