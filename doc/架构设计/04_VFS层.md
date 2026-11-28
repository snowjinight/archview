> [返回目录](README.md)

## 4. 核心抽象：VFS 层

```rust
/// 统一三种压缩包格式的读取接口
trait ArchiveVFS {
    fn open(path: &Path, password: Option<&str>) -> Result<Self>;
    /// 递归列出所有 entry（扁平 + 路径信息）
    fn list(&self) -> Vec<VirtualEntry>;
    /// 打开单个文件，返回可读可 seek 的虚拟文件
    fn open_entry(&self, entry: &VirtualEntry) -> Result<Box<dyn VirtualFile>>;
    /// 决定播放策略的关键能力声明
    fn access_mode(&self) -> AccessMode; // FastSeek | Solid(需缓存回退)
}

/// mpv stream_cb 回调直接对接此 trait；图片查看器同样消费此 trait
trait VirtualFile: Read + Seek {
    fn size(&self) -> u64;
}
```

播放侧注册 mpv 自定义协议（`zvf://archive_idx/entry_idx`），open/read/seek 三个回调
直接转发给 `VirtualFile` —— **mpv 的 seek 就是 VirtualFile 的 seek**，快/慢两条通路对播放器完全透明。
两个关键约束（stream_cb.rs）：

- 回调在 mpv 内部线程执行，**回调内禁止调用任何 mpv API**（死锁）；跨线程状态
  经全局 REGISTRY（`Arc<dyn VirtualFile>`）传递，回调只做纯转发；
- cookie **双重装箱**：`Box<dyn VirtualFile>` 是胖指针（数据指针 + vtable），直接
  `as *mut c_void` 会丢 vtable 导致回调内崩溃——须先 `Box::new(vfile)` 再
  `Box::into_raw`，close 回调按同原型回收。

### 4.1 zip 老包文件名编码（GBK 等）

ZIP 规范（APPNOTE）：文件名无 bit11 UTF-8 标志时按 CP437 解释。老工具（老 WinRAR/好压）
在简中 Windows 上实际按系统 ANSI（GBK）存名字字节——按规范解即乱码，且 zip crate 的
`index_for_name` 以原始字节为 key 查内部表，CP437 解出的字符串 UTF-8 字节必然与原始
GBK 字节失配，`open_entry` 报"entry 不存在"（列表能显示但点开必失败）。

解法（zip_vfs.rs）：`open()` 时顺序扫一遍 CD 收集原始名字节与 flags（一次顺序读，
无逐条 seek；条目数与 crate 不一致的重名折叠等边缘包整体放弃，回退 crate 旧行为），
按链解码：**UTF-8 标志置位或字节为合法 UTF-8 → UTF-8；否则配置码页（config `codepage`，
默认 GBK）；码页解出替换符 → lossy**。解码结果两处消费：`list` 的显示名 +
自建「显示名 → crate 下标」表（`open_entry`/`verify_password` 查找，替代按名字节
查 crate 内部表的 `index_for_name`）。密码与文件名共用同一 `codepage`——老工具
打包时两者同按系统 ANSI 编码，本就同源。VarTidy 同链受益：GBK 名 var 入库不再乱码。

**密码字节的编码约定**（与文件名同源问题）：各格式密码编码约定不一——
7z = UTF-16LE、RAR5 与跨平台工具打的包 = UTF-8、Windows 老工具创建的
zip / RAR4 = 系统 ANSI 代码页。探测时对每个候选密码生成「UTF-8 + 配置码页
（默认 GBK）」两个字节序列依次验证。密码仅存内存，会话内按包路径记忆
（`HashMap<PathBuf, String>`），切换压缩包再切回无需重输，退出即丢。

### 4.2 目录 = 媒体集合（DirVFS）

打开目录不再"寻找目录内的压缩包"，而是把目录本身当作媒体集合：**递归收集其中
图片/视频，跳过压缩包不展开**。收集过滤是**名字谓词**（`is_video_ext(name) ∨
is_image_ext(name)`，与列表过滤 `rebuild_view` 同源）：标准扩展名 ∨
`[[raw.demux]]` 预设 glob 命中——`.yuv420` 等非标名在收集层即可见；压缩包等
其它文件天然被排除（曾用扩展名白名单旁路预设，raw 文件在目录模式下根本
不出现，已统一为同源谓词，复盘见 [DEVLOG §8.6](../架构落地开发/08_RAW媒体支持专项.md)）。
一条规则覆盖全部入口——`initial_archive`（命令行/SendTo/拖到 exe）、拖放进窗口、
「打开目录」按钮，它们都汇聚到 `spawn_open` 的目录分支。混合多选（目录+压缩包同时
「发送到」）按参数顺序取**第一个有效项**打开，其余忽略——语义可预期优先。

**单媒体文件入口**（拖入/SendTo/命令行/「文件 → 打开媒体文件…」选中某个媒体文件）：
文件本身不是打开目标——`open_path` 统一分流为"打开其所在目录的媒体集合 + 记下目标
文件名（`pending_focus`）"。两点边界处理：

- 递归深度默认 **1 层**（`media.file_open_depth`，0 = 不限制）：拖入的文件可能在盘根或
  超大目录里（如 `E:\xxx.mp4`），全盘递归收集不可接受；显式「打开目录」入口仍不限制。
  `DirVFS::open` 为此多了一个 `max_depth` 参数
- 打开完成（`apply_open_done`）消费焦点：按目标扩展名**预切标签页**（混合内容目录下
  目标可能在另一页）→ 按文件名定位条目（ASCII 大小写不敏感——Windows 文件系统语义，
  收集名与拖入名可能写法不同）→ 经 `play()` 跳转播放，列表高亮滚动居中由既有机制接管；
  找不到同名条目静默不播（视图照常打开）。焦点仅在目录模式 Done 时消费（压缩包打开
  不吃焦点，防旧焦点串播）；每次 `open_path` 先重置，防上次残留

实现（`dir_vfs.rs`）：`DirVFS` 实现 `ArchiveVFS`，`access_mode = FastSeek`——本地文件
随机访问 O(1)，与 ZipVFS 同走 `zvf://` 流式快路径，**解压会话/滑窗/缓存/密码探测等
慢路径机制零触碰**（条目全部 `encrypted=false`，密码探测零成本直通）。要点：

- `list()` 递归 walk，条目名 = root 下相对路径（`/` 分隔，与压缩包条目形态一致）；
  **按路径字典序排序**——目录集合没有"包内物理顺序"，`read_dir` 顺序又不可预期，
  字典序是 `ui.sort_files=false` 默认行为下最确定的顺序
- 符号链接 / junction 一律跳过（防环且语义可预期）；子目录不可读静默跳过
  （部分可读的树照常收集）；根目录不可读定性打开失败
- `open_entry()` = `std::fs::File` 套 `VirtualFile` 适配器（size 打开时缓存一次）
- 空集合（目录里没有媒体）打开成功，UI 在 Done 分支统一提示"没有可播放的视频/图片"

行为边界：目录集合模式下没有「同目录下一个压缩包」概念（末尾翻页提示改为一次性
说明）；「目录内连续看包」N 键语义仅在打开压缩包文件时保留。

### 4.3 ISO / UDF 镜像（ISO9660+Joliet 与纯 UDF 2.50，IsoVFS）

第四种 FastSeek 格式（`iso_vfs.rs`）：ISO9660 与 UDF 都是未压缩、未加密的只读
文件系统，每个文件在描述符里直接登记数据区位置（extent LBA）与长度，随机访问 =
打开镜像 `File` + seek 到 `lba*2048 + offset`，O(1)——与 DirVFS/zip Stored 同构，
走 mpv `zvf://` 流式快路径，**解压会话/滑窗/上游预取/密码探测等慢路径机制零触碰**
（条目全部 `encrypted=false`、`crc=None`、`locate_start=None` 默认即正确）。

- **不引入外部库**：VFS 的核心产出物是「文件名 → extent 表」（`VirtualFile`
  适配器的本质），现成 crate（hadris-iso / cdfs / hivepack::iso / hadris-udf）
  都抽象成"按路径读流"，extent 藏在抽象内部且各有坑（hadris 的 Joliet+RR root
  评分选择可能隐藏 Unicode 名、hivepack 顺序流式不适配随机 seek、hadris-udf
  明确拒绝 metadata partition 读不了蓝光）；只读解析手写全权控制（ISO9660 层
  几百行；UDF 布局参照 libfreemkv/udf.rs——对真 BD-ROM 实测过的指针链）
- **双形态识别**：卷描述符扫描（LBA16 起）见 CD001 主卷 → ISO9660 模式
  （bridge 双结构盘走 ISO 侧）；无 CD001 且 AVDP(扇区256) tag=2 → UDF 模式
- **ISO9660 名字空间二选一**：有 Joliet SVD（转义序列 `%/@` `%/C` `%/E`）用
  Joliet（UCS-2BE，原大小写/中文/长名）；否则基础名（大写 8.3，剥 `;版本`
  与尾部 `.`）
- **UDF 2.50 指针链（蓝光 BDMV）**：AVDP → VDS(PD+LVD) → 元数据分区映射
  （type2 map 偏移 62 的 Metadata File Location）→ 元数据文件 FE 的数据
  extent = 元数据分区内容 → FSD(tag 256, 根 ICB long_ad 偏移 400) → FID 树。
  分区语义：ICB/FID/目录数据在元数据分区，**文件数据 short_ad 永远落在物理
  分区**（元数据分区只装结构）——两者换算镜像绝对字节偏移后与 ISO9660 共用
  同一条 extent 通道，`IsoFile` 适配器零改动复用。AD 连续链（extent type 3）
  读指向区域续解（超大 m2ts 的保险）；内嵌数据形态（ICB flags bit3-5 = 3）
  无外部 extent，条目可见但不可播
- **多 extent 合并**（ISO9660）：4GB 上限把大文件切成连续同名目录记录（首条
  flags bit7 置位），合并为一个文件的连续 extent 表；UDF 侧 FE/EFE 的多条
  short_ad 天然多 extent。`IsoFile` 做逻辑位置 → 物理偏移映射（段内起点 =
  前序 extent 长度累计），稀疏/空洞段按 gap→EOF 兜底
- **不参与"同目录下一个压缩包"**：`archives_in`（N 键连续看包）不收 ISO——
  镜像语义是"媒体集合"而非"包"；VarTidy 的包过滤独立维持 zip/7z/rar 不变
- 测试：程序化构造最小合法镜像全链路验证——ISO9660（PVD/SVD/终止符 + 目录
  扇区 + 跨扇区文件数据）与 UDF 2.50（VDS/元数据分区/FSD/FE+261/266 双分支/
  双 extent 跨界 seek/UTF-16BE 名字）各一组
- **L2 规划（完整光盘体验，未实施）**：VOB/M2TS 已入媒体列表（逐文件播正片，
  音视频字幕本就是复用容器无需特殊处理）；跨 clip 无缝拼接/章节/菜单属 L2——
  走 mpv 的 `dvdnav://`/`bluray://` 协议（libdvdnav/libbluray 自己解析 IFO/MPLS，
  不自研），app 侧只需识别（镜像含 `VIDEO_TS.IFO` / `BDMV/index.bdmv` → 呈现为
  单个影片条目）+ 交路径。架构坎：这两个协议要**真实文件路径**不走 stream_cb，
  包中包场景需先把镜像整体落缓存；ISO 直接磁盘打开零解压直通。待办前置：
  实测 bundled mpv 是否带 libdvdnav/libbluray
