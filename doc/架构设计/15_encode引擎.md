> [返回目录](README.md)

## 15. encode 引擎：裸 dump 序列 → 视频（VFS 第三消费者，纯库）

> 形态：**纯引擎库**（`src/encode/`），不含 UI / CLI 入口——壳（后台线程 + 进度
> 展示 / 参数解析）后续按需接入，且与 VarTidy 一样走「与播放器并列的入口」，
> 不塞进播放流程。输入：`Arc<dyn ArchiveVFS>` + dump 条目序列（DirVFS 目录 /
> ZIP / 7Z / RAR 通吃，与播放器同源同能力）；输出：编码视频文件（容器由输出
> 扩展名推断）。完整踩坑叙事（五坑：vo=lavc 隐式切换不存在 / o·ovc 非运行期 /
> bf 命名覆写 / edit list 裁末帧 / 命令串反斜杠）见 **[DEVLOG §9](../架构落地开发/09_encode引擎e2e攻坚.md)**，本节只留
> 机制与设计决策。

### 15.1 数据通路

```
N 个 dump 条目（壳负责按文件名升序排序过滤，引擎只做校验）
  → ConcatFile 逻辑拼接（零拷贝：rawvideo demuxer 只认连续字节流，
    read/seek 按字节区间路由到成员 VirtualFile）
  → zcat://<id> stream_cb 协议（zvf:// 姊妹协议：read/seek/size/close
    四回调直接复用 player/stream_cb，仅 open_cb 为本模块专属）
  → headless mpv（vo=null，与 GUI 播放的 render API 链路完全无关）
     · demuxer 选项走 loadfile 第四参数（与播放链路同构，13.6 同口径）：
       demuxer=rawvideo,demuxer-rawvideo-w/h/mp-format/fps
     · 编码选项走全局选项（mpv_initialize 前下发）：o=<输出> ovc=<编码器>
       ovcopts=<参数>（+ mp4 系 ofopts=use_editlist=0，见 15.2）
  → END_FILE(EOF) = 编码完成 → 回读自校验
```

引擎对外仅一个入口 `encode_raw_sequence`（阻塞直至完成/失败/取消），格式参数
`EncodeFormat`（pixfmt/宽高/fps/bit_shift）、编码参数 `EncodeOptions`
（ovc/ovcopts，默认 libx264）、统计 `EncodeStats`（文件数/字节/帧数）。
格式参数来源 = `[[raw.demux]]` 预设命中值或壳显式输入，由壳负责解析。

### 15.2 关键设计

- **create_uninit / initialize 拆分**：o / ovc / ovcopts / ofopts 是非运行期
  选项，必须在 `mpv_initialize` 前下发——与播放链路（create 即初始化）刻意
  不同，mpv_ffi 为此新增 `create_uninit` 符号。
- **默认 bf=0 + mp4 系 use_editlist=0（配套）**：mpv encode 路径包 duration=0
  → movenc 按 PTS 差推算末样本时长 0 → edit list 总时长少一帧 → libavformat
  系解码器按 elst 裁末帧（e2e 实证 10 帧只解出 9）；而 x264 默认 B 帧的负 DTS
  会让 muxer 写 elst 补偿——只修其一仍裁帧。bf 必须用 ffmpeg 通用名（x264
  私有名 bframes 会被 libx264 包装层用 avctx 覆写，设了不生效）。用户 ovcopts
  显式指定 bf=/bframes= 时以其为准。细节见 [DEVLOG §9](../架构落地开发/09_encode引擎e2e攻坚.md) 坑3/坑4。
- **校验先行**：pixfmt 名单（config raw.pixfmts）、帧对齐（总字节须为帧大小
  整数倍，不匹配即报"序列与格式参数不匹配或混入非 dump 文件"）、fps/宽高/
  bit_shift 越界、加密条目拒绝——错误文案与播放链路 raw_demux_opts 同口径。
- **bit_shift 流层修正（ShiftReader）**：12bit 低对齐数据存 16bit 容器的高位
  修正，语义同播放链路——修不正的数据编进去也是废片，故在流层先修正再喂 mpv。
- **取消 / 进度**：`cancel: &AtomicBool`（置位即 stop，返回「已取消」；半成品
  输出不删除，交壳决定——失败现场有调试价值，重试会覆盖）；`progress:
  &dyn Fn(f64)`（0~100，引擎线程回调，壳自行处理跨线程）。开始前已取消则
  不加载 dll 直接退出（零成本路径）。
- **回读自校验**：编码完成后起第二个 headless mpv 回读输出文件（宽高/时长），
  e2e 另有 ffprobe 精确帧数断言（三级工具探测：PATH → `ARCHVIEW_TEST_FFMPEG`
  环境变量 → 固定 fallback，惯例同 rar_vfs 的 rar/ffprobe 探测）。

### 15.3 复用清单（引擎并入本仓库的依据）

| 复用能力 | 来源 | 说明 |
|---|---|---|
| stream_cb 四回调 + REGISTRY | `player/stream_cb.rs` | zcat 仅换协议名与 open_cb，回调桥接/装箱约定原样复用（§4 两约束） |
| mpv 最小 FFI | `player/mpv_ffi.rs` | 新增 create_uninit；dll 加载/错误处理同源 |
| rawvideo demuxer 选项口径 + raw_pixfmt_bits | `config.rs` + 13.6 | 与播放 raw 链路同一套 pixfmt 名单与位深表 |
| 输入能力（四 VFS + 密码探测 + 分卷 RAR） | `vfs/` 全部 | dump 序列在压缩包内或磁盘目录均可编码 |

单向依赖：encode → vfs / player / config，零反向依赖；与播放器、VarTidy 互不感知。

### 15.4 实施状态

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1 | 引擎本体：ConcatFile / zcat:// / headless encode / 校验 / 取消 / 进度 / 回读校验 | ✅ 完成（4 测试：拼接读越界/seek 语义/校验取消/e2e 帧级校验；全量 113 通过；main.rs `#[allow(dead_code)]` 占位，壳接入后移除） |
| 2 | CLI + SendTo 薄壳（设计已定型，见 15.5） | ✅ 完成（`archview encode` 子命令：输入解析/唯一族校验/自然序/连续性缺口警告/输出命名拒绝覆盖/Out+msgbox 自实现；11 测试；sendto_setup.bat 第三项） |
| 3 | UI 薄壳（设计已定型，见 15.5） | ✅ 完成（列表多选 Ctrl/Shift/Ctrl+A + 右键「编码为视频…」；单选确认窗二选一（整族/仅此文件）；后台线程+进度窗+取消；完成绿条点击打开输出目录；2 测试：多选语义/切包清空） |
| 4（二期） | 图片帧源适配器：jpg/png/webp 等逐帧解码 → 统一像素流；**分辨率不一致时算法归一**（缩放拉齐，不再一票否决）——raw 路径维持严格校验不变 | 规划（壳按帧源无关设计（15.5），二期只加收集规则 + 帧源适配器，壳零改动） |

### 15.5 壳设计（CLI / SendTo / UI，已定型）

**输入语义总纲：显式选择即精确，扩展是显式动作**（用户选什么就编什么，序列扩展为目录/确认窗里的显式选择）。

| 通道 | 输入 | 语义 |
|---|---|---|
| CLI / SendTo | ≥1 个帧文件 | 精确模式：所见即所选（多文件参数 = **一个序列的成员**，区别于 tidy 的多任务语义）；混合输入类型（文件+目录）报错 |
| CLI / SendTo | 目录 | 唯一族校验：恰好一族 → 全族编码；多族 → 报错并列各族清单（预设 glob / 文件数 / 参数摘要），提示选中任一帧文件重发 |
| CLI / SendTo | 压缩包（zip/7z/rar） | 一期即支持：VFS 打开 + 条目名匹配预设（`glob_match` 已做路径剥离），压缩包内 dump 免解压直编 |
| UI 列表 | 多选（≥2） | 精确模式（同 CLI） |
| UI 列表 | 单选 → 确认窗 | 窗内二选一：「整个序列 (N 帧)」默认 /「仅此文件」 |

- **预设锚点**：选中文件命中的 `[[raw.demux]]` 预设 = 族过滤器——多族目录天然分流（jpg 不会命中 raw 预设的 glob，二期图片混放自动隔离），**config 不加临时屏蔽开关**。预设已含全套参数（match/pixfmt/宽高/fps 默认 3/bit_shift，见 §7），编码链路直接消费，零 config 改动。
- **SendTo 上限与防呆**：SendTo 机制 = explorer 拼命令行，受 CreateProcess **32767 字符硬上限**（短路径 ~1400 文件、长路径 ~300），且超限时 explorer 可能**静默截断**——对帧序列即静默丢尾帧，且引擎帧对齐校验拦不住（丢整倍数尾帧照样通过）。两道对策：壳层**编号连续性校验**（自然序排序后提取文件名编号轴，缺号报告断点后**警告不中止**——稀疏 dump 是合法场景，编号只是提醒）；大序列正道 = **发目录**（单参数零压力）。
- **自然序**：壳统一用 `alphanumeric-sort` 重排（SendTo 多选传参顺序不可靠，必须壳内重排）；引擎契约不变（"entries 已按编码顺序排列"）。
- **输出**：序列所在目录 + 首帧基名（`dump_0000.nv12 → dump_0000.mp4`）；已存在**拒绝覆盖**（提示改用 `-o`）。CLI：`archview encode <输入...> [-o out] [-q]`，编码参数不进命令行。
- **config `[encode]` 节**（新增，默认值可调）：vcodec（默认 libx264）、crf、preset 等 → 组装 ovcopts 下发引擎；用户 ovcopts 显式指定 bf/bframes 时以用户为准（15.2）。
- **反馈**：有控制台打印进度行；结束弹结果窗（耗时/输出路径/帧数）+ `-q` 静默（tidy 惯例）。SendTo 一键项「Archview encode」（bat 模板第三项）。
- **UI 执行**：列表 raw 文件右键「编码为视频…」→ 确认窗（族信息/文件数/总帧数/宽高/pixfmt/fps 可改/输出路径）→ 后台线程跑引擎（复用任务+代际号模型）→ 状态行进度 + 取消 → 完成绿条（点击打开所在目录）。编码 mpv 实例独立（vo=null + zcat 独立 id 空间），编码期间播放不受影响；同时仅一个编码任务。
- **帧源无关**：壳的收集→排序→参数→启动流水线不含"文件是 raw"的假设，二期图片帧源（+分辨率归一）只加收集规则与帧源适配器，壳与引擎签名零改动。
