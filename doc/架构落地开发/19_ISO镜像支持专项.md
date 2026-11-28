> [返回目录](README.md) · 对应架构设计：[ARCHITECTURE §4.3](../架构设计/04_VFS层.md)

## §19 ISO 镜像支持专项（ISO9660 / Joliet / 纯 UDF 2.50 蓝光）

ISO 镜像 VFS（`iso_vfs.rs`）从零落地的专项记录：一期 ISO9660+Joliet 手写解析、
二期 UDF 2.50 纯蓝光盘、L1 扩展名接入（vob/m2ts/mts）、安装集成（ProgID + 专属
图标）。设计定位与 UDF 指针链机制见 ARCHITECTURE §4.3，本文只记"怎么做的、
坑在哪、怎么排查"。

### 19.1 crate 调研与否决记录

| 候选 | 否决理由 |
|---|---|
| hadris-iso / cdfs | 抽象成"按路径读流"，extent 藏在抽象内部；hadris 的 Joliet+RR root 评分选择可能隐藏 Unicode 名 |
| hivepack::iso | 顺序流式设计，不适配随机 seek |
| hadris-udf | 明确拒绝 metadata partition，读不了蓝光盘 |

核心产出物是「文件名 → extent(镜像绝对字节偏移, 长度)」表——这是 `VirtualFile`
适配器的本质，现成 crate 没有一个交出这张表；只读解析手写几百行即可全权控制。

### 19.2 实现要点

- 双模式同一条数据通道：解析产出 extent 表 → `IsoFile` 适配器做逻辑位置 →
  物理偏移映射（O(1) seek）→ mpv `zvf://` 快路径；慢路径机制零触碰
- ISO9660：PVD(LBA16) + Joliet SVD（转义序列 `%/@` `%/C` `%/E`）二选一名字空间；
  多 extent 合并处理 >4GB 大文件（首条目录记录 flags bit7 置位 + 连续同名记录）
- UDF 2.50：VDS 扫描(32..64) 取 PD(partition_start@[188..192]) / LVD(num_maps@[268..272])；
  FE(261)/EFE(266) info_length 都在偏移 56；short_ad = len+lba（高 2 位 extent 类型，
  type 3 = AD 连续链续解，上限 8 次防死循环）；ICB flags bit3-5 = 3 为内嵌形态
  （条目可见但不可播）；目录 walk 深度 32、单段 8MB 上限防畸形盘

### 19.3 坑点清单

**坑 1：UDF type2 元数据分区映射字段偏移记忆错误（真盘蓝光盘打不开，最大坑）**

- 现象：真盘（SMBD-31 纯 UDF 2.50，22.4GB）打开报 "FSD 缺失"，fixture 测试却全过
- 根因：把 Metadata File Location 记成"偏移 62 的 long_ad"，实际读 66..70 落在
  镜像位置字段 → FE 定位到全零扇区 → metadata_start 退化 → FSD 读空。
  **真实布局：map 长 64 字节，偏移 40..44 = Metadata File Location（物理分区
  相对裸 u32）**，44..48 = 镜像位置，48..52 = 位图位置（-1 = 无）
- 修复：读偏移 40 裸 u32；为 0 时退化试物理分区 lba 0（兼容 libfreemkv 硬编码
  形态——lba 0 全零的盘该假设不成立，实盘元数据文件在分区相对 148）
- 教训：**UDF 2.50 规范记忆不可靠，以真盘 hex dump 为准**。诊断路径：AVDP →
  VDS 逐扇区 dump → map2 原始字节逐字段比对 → FE tag/info_len 验证。真盘证据：
  `02 40 00 00 00 2A "UDF Metadata Partition" 50 02 ... 94 00 00 00 ...`

**坑 2：BDAV m2ts 的 192 字节节奏（数据校验误报）**

- 现象：按 TS 标准 188 字节节奏校验同步字节，真盘 m2ts 全部误报"TS 同步失败"
- 根因：BDAV MPEG-TS 每包 = 4 字节 TP_extra_header + 188 字节 TS 包，0x47 在
  每 192B 单元**偏移 +4**（不在 +0）
- 修复：校验按 192B 节奏查 +4；头部/中部各验一段

**坑 3：bridge 与纯 UDF 双形态（"旧版能开 A 盘打不开 B 盘"的差异根源）**

- bridge 桥接盘（ISO9660+UDF 双结构，有 CD001 主卷）走 ISO 侧——新旧版本都能开；
- 纯 UDF 盘（无 CD001，BEA01/NSR03/TEA01 标记 + AVDP@256）走 UDF 指针链——
  坑 1 修复前打不开。两种形态用户侧无法从扩展名区分，报错文案需带诊断信息
  （FSD 缺失报错附 metadata_start/partition_start）

**坑 4：BD 菜单流小 m2ts 打不开属 mpv 内容层限制（非解析问题）**

- 真盘 BDMV/STREAM 下 00002.m2ts(829KB)/00003.m2ts(116KB) mpv 播不出：BD 菜单
  导航流内容 mpv 不兼容；正片（00001.m2ts 21.8GB）数据面校验全绿。ISO 模式新旧
  exe 代码完全一致，不要往解析层排查

**坑 5：fixture 数据布局冲突**

- 构造 UDF 测试镜像时 extent1（3000B）尾部与 extent2 同扇区重叠 → extent2 挪到
  不重叠的物理分区相对扇区；`img[N] = [u8;2048]` 整体赋值类型不匹配 →
  `copy_from_slice`

### 19.4 L1 扩展名接入

- `vob` `m2ts` `mts` 加入 `video_extensions` 默认表与 DEFAULT_TOML 模板
- **注意**：老 config.toml 已有 `[media]` 段的不会自动获得新项（TOML 键级合并
  语义，缺键才补全）——用户需手动加三项或删除 `[media]` 段重建默认配置

### 19.5 安装集成（install.bat / uninstall.bat / 图标）

- 独立 ProgID `Archview.Iso`（"Archview Disc Image"）：ISO 有专属图标，不能
  复用 `Archview.Archive`（其 DefaultIcon 指向压缩包图标）
- 图标：`target/gen_iso_icon.ps1`（PowerShell GDI+ 绘制银色光盘 + 蓝色数据弧，
  多尺寸 256/64/48/32/16 PNG-in-ICO 装配）→ `assets/icons/iso.ico` →
  build.rs `set_icon_with_id(..., "5")` → exe DefaultIcon **零基索引 4**
- 教训：ps1 含中文注释 + UTF-8 无 BOM 时 PS 5.1 按 GBK 解码，Add-Type 加载
  System.Drawing 失效（单测脚本无中文则正常）——**脚本一律 ASCII-only**
  （用户约定），见 §15 的 ANSI 乱码同源问题
- install.bat 加 `.iso` 关联 + echo 列表；uninstall.bat 删 ProgID + 摘
  OpenWithProgids 值

### 19.6 测试与排查

- 回归测试：fixture 全链路（ISO9660 + UDF 双分支）+ 全量 cargo test；
- 真盘诊断：`#[ignore]` 测试 + `ARCHVIEW_ISO_TEST=<iso路径>` 环境变量门控——
  列目录树 + 对全部 .m2ts 做 192B 节奏 0x47 校验；`target/udf_probe*.rs` 为
  真盘诊断探针（保留备查）
- 排查对照（grep 日志）：

| 现象 / 报错 | 定位 |
|---|---|
| `不是 ISO9660 镜像` / `不是 ISO9660/UDF 镜像` | 文件过小（<17 扇区）或卷标全不匹配，非镜像/损坏 |
| `UDF 镜像结构异常（AVDP 缺失或损坏）` | 扇区 256 tag ≠ 2，纯 UDF 判定失败 |
| `UDF 镜像结构异常（VDS 不完整…）` | 32..64 扫不到 PD(tag5)/LVD(tag6)，镜像被截断或非 2048 扇区 |
| `不支持的逻辑块大小: N` | PVD 声明块大小 ≠ 2048（媒体 ISO 恒为 2048） |
| `FSD 缺失`（带 metadata_start/partition_start） | 坑 1 同款：type2 map 字段偏移或元数据文件定位失败 |
