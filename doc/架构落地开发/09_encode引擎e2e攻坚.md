> [返回目录](README.md)

## 9. NV12 dump→视频 encode 引擎 e2e 攻坚（src/encode/，2026-11-02）

ConcatFile 拼接流（复用 zvf 协议无关四回调，注册为 `zcat://`）喂
headless mpv，`demuxer=rawvideo` 系列选项 + `o/ovc/ovcopts` 编码输出。
链路打通共排掉五个坑，全部有实证：

**坑 1：运行时 API 没有 `--o` 的隐式 vo 切换**。命令行 `mpv --o=x.mp4`
会隐式把 vo 切到 lavc，但 `mpv_set_option_string` 不做该切换——vo 留
null 时文件被当普通播放消费（END_FILE 正常 EOF 但零输出），且换
vo=lavc 后报 "No video or audio streams selected"。必须显式
`vo=lavc,ao=null`。

**坑 2：o/ovc 是非运行期选项**。在 `mpv_initialize` 之后设置不生效
（encode 上下文不创建，报 vo 初始化失败）。为此把 `mpv_ffi::create`
拆成 `create_uninit()` + `initialize()`（播放链路 create() 行为不变），
encode 侧选项全部就位后才 initialize。

**坑 3：ovcopts 的 x264 私有名会被覆写**。`bframes=0` 设了不生效
（x264 参数行仍 bframes=3），而 crf=28 同串生效——对照实验证明 ovcopts
通道本身是通的、bframes 被单独覆写：ffmpeg libx264 包装层用
AVCodecContext.max_b_frames 覆写私有 i_bframe。必须用 ffmpeg 通用名
`bf=0`。引擎默认追加 `bf=0`（用户显式指定 bf=/bframes= 时不覆盖）。

**坑 4：edit list 裁掉末帧（最隐蔽）**。现象：容器 nb_frames=10、包
10 个、PTS 网格完整，但 mpv/ffmpeg 解码都只出 9 帧，丢的是最后一帧。
链路：mpv 编码路径发出的包 duration=0 → movenc 按 PTS 差推算样本
时长，末样本时长 0 → track/elst 总时长少一帧（0.9s 而非 1.0s）→
libavformat 系 demuxer 按 elst 裁掉 PTS≥21600 的末包。实证：
`ffmpeg -ignore_editlist 1` 可解出全部 10 帧。修复：mp4 系输出
`ofopts=use_editlist=0`（bf=0 已消除负 DTS，本就不需要 elst 补偿）。
注意坑 3 的 bf=0 与坑 4 配套：只修其一时 bframes 路径仍因负 DTS 写
elst（media_time=4800）裁帧。

**坑 5：命令字符串引号内 `\` 是转义符**。`loadfile "C:\Users\..."`
报 "invalid parameter"——mpv 命令解析器吃掉反斜杠。路径须转正斜杠。

**验证方法**：e2e 用纯 Rust 造 10 帧 64×48 nv12（每帧恒定亮度 i*25）
→ 编码后第二 mpv 实例回读 width/height/duration（核心断言零外部
依赖）→ ffprobe 精确帧数（可选增强，三级探测：PATH →
ARCHVIEW_TEST_FFMPEG → `D:\11_software` 固定 fallback，rar_exe 同款
模式）。回读失败与丢帧靠亮度序列（0,25,…,200 解出、225 丢失）定位。

113 passed（基线 109）：encode 新增 4 测（ConcatFile 跨界读/seek 语义 /
校验+取消 / e2e）。壳（UI/CLI）未接入，mod 声明处 `#[allow(dead_code)]`
占位，接入后移除。

> ⚠️ 现状补记：上文"壳未接入"为当时快照——三壳已全部接入
> （见 [§10](10_encode三壳接入.md)），`main.rs` 占位已移除。
