> [返回目录](README.md)

## 5. Seek（拖动进度条）详细设计

### 5.1 结论

**完全支持。** 进度条拖动在 mpv 中即 `seek` 命令 → demuxer 将时间换算为字节偏移 →
触发 stream_cb 的 seek 回调 → `VirtualFile::seek()`。链路完整，差异仅在性能，
按下表分级处理。

### 5.2 各格式 seek 成本

| 格式/模式 | 下游 seek（向文件尾） | 上游 seek（向文件头） | 说明 |
|---|---|---|---|
| ZIP (store) | O(1) | O(1) | 直接偏移定位 |
| ZIP (deflate) | 便宜（跳读解压输出） | **贵**（从 entry 头重新 inflate） | 需解码块缓存缓解 |
| 7Z 非固态 | 便宜 | **贵**（LZMA 从 entry 头重解） | 同上 |
| RAR 非固态 | 便宜 | **贵** | 同上 |
| 任意格式固态（慢路径） | 需解压推进到目标点 | 同左 | seek = 解压任务重定向 |

> 多 GB 视频在 deflate/LZMA 下的上游 seek 若无缓存，可能需数秒 —— 因此以下缓解措施为**必选组件**。

### 5.3 缓解措施（四层）

1. **mpv demuxer 缓存**：调大 `demuxer-max-bytes` / `cache-secs`（config.toml 可配）。
   小幅回跳（刚看过的地方）命中缓存，零成本 —— 覆盖最常见的拖拽场景。
2. **解码块 LRU 缓存（快路径）**：解压后的数据按块（默认 4MB/块，可配）进 LRU；
   上游 seek 落在缓存内则 O(1)。播放为顺序读，"拖回刚才看过的地方"命中率极高。
3. **解压任务重定向（慢路径/固态）**：seek 超出已解压区域时，重定向解压会话的目标集
   （见 11.4），不终止解码流。UI 状态栏显示"跳转中…"。
4. **读取阻塞即缓冲**：stream_cb 的 read/seek 回调在 mpv 缓存线程上同步执行，
   阻塞是安全的（与网络流一致），mpv 自动显示 buffering 动画。

### 5.4 交互细节

- UI 进度条：观察 mpv `time-pos` 属性驱动进度显示；拖动松手后发 `seek` 命令（拖动中不连续发）。
- 慢 seek 期间：UI 显示 mpv buffering 状态；连续拖动产生的新 seek 取消前一个解压重定向任务。
- 快路径上游 seek 且缓存未命中：在 read 回调中同步重解压，期间 mpv 自然缓冲。

### 5.5 逐帧步进与方向键长按连发

暂停态 ←/→ 逐帧步进、播放态 ±5s seek，两类操作都必须**不触碰 pause 属性**：

- **前向逐帧用 `frame-step 1 seek`（flags=1）而非默认 flags=0**。默认语义在
  mpv 内部是"真解除暂停播一帧再重新暂停"（playloop.c `add_step_frame` 先
  `set_pause_state(false)`、video.c `step_frames` 归零时再 `set_pause_state(true)`，
  且 `set_pause_state` 末尾**无条件**推 pause 属性通知）——两次推送经
  `observe_property` 写 `paused_cache`，控制栏图标闪"播放→暂停"，且瞬态
  `pause=false` 会让下一次按键被误判为播放态。flags=1 走 `MPSEEK_FRAMESTEP`
  精确 seek + `set_pause_state(true)`，全程保持 pause=yes，与 `frame-back-step`
  （内部即 seek 语义）对称。
- **mpv 端背压与完成驱动链发**：步进 seek 的完成标志是 `PLAYBACK_RESTART`
  （目标帧已显示）。长按连发据此**完成驱动**——上一发未完成不发下一发
  （`step_idle` 闸门 + 泵线程事件唤醒），节奏自适应 mpv 逐帧消化速度、
  无丢命令错拍；33ms 为最快上限（≈30fps）。异常无 RESTART（如 EOF 尾帧）
  由 400ms 超时强发兜底。
- **播放态连续 ±5s**：mpv 对 `MPSEEK_RELATIVE` 入队**累加合并**（`seek->amount
  += amount`），连发速率快于消化速度时自动合并为一次大步 seek，安全。
- **长按连发事件模型**（见 [DEVLOG §13](../架构落地开发/13_空闲CPU全事件驱动专项.md)）：
  key-down 首拍落地 + 记录方向与节奏锚点，key-up 清除，按住期间每帧检查到点
  即发并 `request_repaint_after` 排下一发（全事件驱动模型下与幻灯片同类的
  timer 排帧场景）；失焦（`WindowFocused(false)`）/模态（编辑器、对比、搜索框
  聚焦、播放器离场）即清除——这些场景 key-up 必然收不到，按住状态不可信。
