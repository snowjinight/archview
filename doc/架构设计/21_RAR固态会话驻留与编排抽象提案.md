# 21. RAR 固态会话驻留与编排抽象提案

> 状态：**提案（待实施）**。背景：7z 固态会话已完成"驻留暂停 + 唤醒续解 +
> 唤醒段骨架 + 续解标志"全套（§11.4）；RAR 固态包仍是"批解净即退出线程、
> 下次点击从头遍历重解前缀"的旧模型（§11.2 会话化最初形态）。本节记录
> RAR 移植路径、与 7z 的模型差异对照、编排层抽象的取舍评估与分阶段计划。

## 21.1 现状模型复盘

**RAR solid 会话结构**（[rar_vfs.rs](../../src/vfs/rar_vfs.rs)）：

```
extract_session（trait 协议入口）
 └─ 外层 loop（规划轮）
     ├─ 持锁预检：cancel → 报错退出；targets 空 → alive=false 退出线程
     ├─ 候选循环（密码/编码）× Restart 循环（focus 回跳，上限 64）
     │    └─ session_pass：打开 unrar Open → 顺序 read_header 遍历
     │         ├─ 每条目持锁快照：目标？上游漂移（focus < cur 且 focus 仍待解）？
     │         ├─ 非目标 → skip（solid 真解码）+ skip_note 刷新 + 分段进度
     │         ├─ 目标 → extract_to(tmp) + rename + file_done（幂等）
     │         ├─ targets 空 → PassFlow::Done 提前收工（★无驻留，Open 随之销毁）
     │         └─ 遍历尽 → PassFlow::Done
     └─ 无进展保险：连续两轮零落地且 targets 非空 → 报错
```

关键点：`session_pass` 里 `targets.is_empty()` 即返回 `PassFlow::Done`
（[:455-457](../../src/vfs/rar_vfs.rs#L455-L457)），外层下一轮预检再次判空 →
`alive=false` 线程退出。**批解净 = 解码器销毁**——下一次点击重新 spawn 线程、
重新 `open_for_processing()` 从头逐条 skip 定位，solid 前缀是真解码，
分钟级（与 7z 驻留改造前同病）。

**7z 会话已就绪的能力**（[sevenz_vfs.rs](../../src/vfs/sevenz_vfs.rs)，§11.4）：

- 批边界 `pause_at_batch_boundary` 分流：身前有目标 `Ok(true)` 顺流续解 /
  否则 `Ok(false)` 中断回外层重排（folder 精判）；
- targets 空后线程**条目边界真暂停驻留**（解码器不销毁，睡眠等新目标，
  唤醒自动续跑）；
- 唤醒臂 `wake_skeleton`：新鲜 focus/targets 重算段骨架 + 开工协议 +
  from=唤醒点+1 防分母虚增；
- `SessionState.resume` 续解标志（规划置 false / 唤醒臂置 true），UI 顶栏
  与覆盖层区分"续解中 / 顺序解压"。

## 21.2 模型差异对照（RAR ≠ 7z 的地方）

| 维度 | 7z solid | RAR solid |
|---|---|---|
| 定位方式 | folder 直达（头部偏移 O(1)） | 顺序 read_header 逐条 skip（solid 真解码） |
| 暂停点 | 解码流条目边界 | read_header 循环条目边界（**Rust 侧，非 C++ 回调内**） |
| 上游目标 | 跨 folder O(1) 重排直达 | **只能 Restart 回跳**：重新 open 从头重解（物理约束，无 seek） |
| 下游目标 | 同 folder 身前有目标 → 顺流续解 | 新批在遍历位置下游 → **接着遍历续跑（零前缀重解，驻留核心收益）** |
| 分流判定 | folder 精判（身前 idx > cur） | 简单上/下游判定（无 folder 概念） |
| 驻留持有 | BlockDecoder（进程内存） | unrar Open 对象（句柄 + 解码状态，**待验证**） |
| 骨架重发 | wake_skeleton（元数据求和） | 复用 pass 开工的 phase_totals 重算 |
| 进度粒度 | 解码流逐段实时 | 段基线（unrar 无逐字节回调，写前发基线、完成发全量） |

**核心结论**：RAR 的唤醒判定比 7z 简单——无跨 folder 分流、无 folder 序
规划；**"下游目标续跑 / 上游目标 Restart"** 一条判定即可。驻留的收益
场景是下游点击（约等于原 7z 唤醒续解）；上游点击的前缀重解是 unrar
物理约束，驻留救不了（与 7z 的"上游漏网项禁止前缀重解"教训同理：
能省的省，省不了的明确不省）。

**"能否精确步进到 folder 起始点"调查记录**（2026-11-28，vendored unrar
源码 + 格式考证）：

- **容器头可随机导航**：unrar 内部就在条目间 seek（`extract.cpp:459`
  `Arc.Seek(Arc.NextBlockPos-Arc.FileHead.PackSize, SEEK_SET)`）；listing
  零解码。WinRAR Quick Open（.qo）正是利用此特性加速列目录。
- **solid 是逐条目链，不是整包铁板**：RAR4 每文件头 `LHD_SOLID 0x10`、
  RAR5 `FCI_SOLID 0x40`；解码器 `UnpInitData(Solid=false)` 全量重置
  （窗口指针 `UnpPtr=WrPtr=0`、距离表、Huffman 表重读）——非 solid 条目
  就是链重同步点。WinRAR `-s<N>`（每 N 文件断链，即 GUI "固态块大小"
  选项，多链包在野包中真实存在）/`-sfe`/续加文件会产生多条独立链，链
  边界语义与 7z 块边界同构。
- **但典型 `rar a -s` 整包一条链**：仅首条目 solid=0，其余全续链；
  folder 只是名字前缀，不是 7z 那样的物理独立单元。且多链包在 unrar
  API 路径下同样不省成本——skip 是否解码看主头 solid 标志而非逐条目标志，
  前面链的条目照样逐条真解码丢弃，链边界只有换 `Read+Seek` 读取器才
  能兑现收益。
- **API 死结**：unrar crate 0.5.8 无条目定位能力，skip=解码丢弃（连非
  solid 条目的 skip 也走 unpack 路径）；即便包内有链边界，前面的链也只能
  skip 解码越过。crate 已解析每条目 SOLID 标志进内部 `EntryFlags` 但未
  公开（Entry 无 `is_solid()`）。
- **结论**：典型 `-s` 包从头解是物理约束，驻留模型已是最优。可选增强
  （暂不做，留档）：换纯 Rust 读取器 unrar-rs（`Read+Seek` 容器 + RAR5
  全套解码）理论上可"头级 seek 跳过前面的链（零解码）+ 从链起点开解"，
  但仅支持 RAR5 且换库成本高。

## 21.3 RAR 驻留设计

**暂停点选择**：`session_pass` 的 read_header 循环内、条目边界处——
read_header 返回后、skip()/extract_to() 调用前。此刻控制权在 Rust 侧，
不在任何 unrar C++ 回调内，阻塞睡眠只是持有 Open 对象的普通线程睡眠。
（记忆中的风险"unrar C++ 回调内长时间阻塞"针对回调内阻塞；条目边界
驻留天然规避，但仍需 Phase A 实证。）

**分流语义**（替换现有 `targets.is_empty() → PassFlow::Done`）：

```
条目边界（read_header 返回 Some 后）：
  targets 非空 → 正常推进（现有逻辑不变）
  targets 空   → 驻留分流：
    ├─ 持有 Open，进入睡眠循环（短锁周期性检查 targets/cancel；
    │   醒来重读纪律：禁止沿用睡眠前快照决策）
    ├─ 唤醒后 resume=true + 重算 phase_totals 骨架（复用 pass 开工代码）
    ├─ 新批全部在当前遍历位置上游 → PassFlow::Restart（回跳重启，
    │   前缀重解不可避免）；驻留先落盘不白费（幂等跳过）
    └─ 新批有下游目标（含 min(targets) ≥ cur）→ 继续当前遍历
        （零前缀重解，相当于 7z 唤醒续解）

遍历尽（read_header 返回 None）且 targets 空：
  → 同样进入驻留睡眠（但唤醒后必然 Restart——游标已耗尽）
  → 或直接按现有外层语义退出线程（二选一，Phase B 实测资源占用后定，
    倾向前者以保住"尾部点击下游"的场景）
```

**外层 `extract_session` 改造**：预检判空不再退出线程（改为让 pass 内
驻留接管）；线程生命周期 = 整个 app 会话期（与 7z 对齐）。cancel 仍是
唯一硬退出。候选循环/Restart 上限/无进展保险维持现状。

**UI/协议侧**：零改动——`SessionState`（targets/focus/alive/gen/
skip_note/resume）与五段开工协议完全复用；RAR 版 wake 臂只需按 RAR 语义
重算 phase_totals（现有 session_pass 开工代码下放成可复用函数）。

## 21.4 编排层抽象评估

两套会话的编排骨架重复度已很高：预检（cancel/targets 空语义）、规划轮
快照 + 骨架开工协议、resume 标志置位、skip_note 刷新协议、落地幂等 +
file_done、无进展保险、驻留睡眠循环、wake_skeleton 重发。差异集中在：
定位/推进原语（BlockDecoder folder 直达 vs unrar 顺序遍历）、分流判定
（folder 精判 vs 上/下游判定）、重启形态（外层重排 vs Restart）。

- **方案 A（trait 编排层）**：`SessionDriver` trait（规划/推进/分流/唤醒
  四钩子），一个泛型 `session_orchestrate<D>` 收编外层 loop。
  风险：7z 的 folder 遍历序、RAR 的候选×Restart 双层循环差异大，强行
  统一会把差异挤进钩子参数，钩子签名被两边少数服从多数地拉扯。
- **方案 B（共享函数提取，推荐）**：不设 trait，把可独立命名的共性收编
  为 `vfs` 模块自由函数/小结构：`session_prelude`（预检+快照）、
  `park_until_wake`（驻留睡眠循环，闭包出参=醒来原因）、
  `wake_skeleton` 通用化（按 VFS 提供求和闭包）、resume/skip_note 协议
  函数。两 VFS 保留各自 loop 形态，重复行数降到每边几十行以内。
  优点：增量安全、不预设错误抽象；待 Phase B 落地后若 loop 骨架仍高度
  镜像，再评估升级方案 A（YAGNI 原则：先让重复出现，再收编重复）。

## 21.5 风险点

1. **unrar Open 长时间闲置安全性（Phase A 必须先证）**：睡眠数分钟后
   read_header/skip/extract_to 是否正常、解出数据 CRC 是否完整、线程
   是否真睡眠（不占 CPU）。unrar 是解压器不是服务器，理论上静态持有
   无害，但 C++ 内部缓冲/句柄行为需实测。若不安全 → 退路：遍历尽即
   退出线程 + 保留现有模型，仅做编排层共享函数提取（收益缩水但零风险）。
2. **驻留资源占用**：56G 包的 Open 状态（句柄/映射/解码缓冲）常驻。
   7z 驻留持有 BlockDecoder 同量级，预期可接受；实测确认。
3. **Restart 与驻留交互**：上游目标触发 Restart 时已驻留落盘的条目
   幂等跳过（现有机制），但 Restart 上限 64 的计数语义需把"驻留唤醒
   导致的 Restart"与"漂移抢道 Restart"合并计数，防 UI 连续上游点击
   打满上限误报错。
4. **skip_note/resume 协议一致性**：驻留睡眠期间 skip_note 应保持最后
   状态还是清空（7z 驻留清空）——对齐 7z：驻留即清空。

## 21.6 分阶段实施计划

- **Phase A — 安全性验证 spike**（先行，不进主干）：测试内构造 rar
  固态包 fixture（现有 rar 测试资产复用；unrar 无法压缩，需固定
  fixture 或 rar 工具生成脚本），open → 解几条 → 条目边界睡眠 30s+ →
  继续解完 → 逐条 CRC 校验 + 线程 CPU 占用断言。**通过才进 Phase B。**
- **Phase B — RAR 驻留最小实现**：按 §21.3 改 session_pass 分流 +
  外层预检；wake 臂（骨架重算 + resume=true）；驻留清 skip_note；
  Restart 计数合并。回归测试镜像 7z 组：`rar_session_parks_at_entry_boundary`、
  `rar_session_wake_downstream_resumes`（判据=唤醒后无 Restart、
  Downstream 骨架非零）、`rar_session_wake_upstream_restarts`。
- **Phase C — 编排层方案 B 收编**：提取 `session_prelude` /
  `park_until_wake` / wake 骨架闭包化，两边 loop 改调共享函数；
  完成后复评是否值得升级方案 A。
- **Phase D — 文档与状态同步**：本节状态改"已实施"；§11.4 补 RAR 驻留
  描述；落地开发 01/04 同步叙事与排查；README 推荐阅读路径补 RAR。

依赖顺序 A → B → C → D，B 可独立发版（C 纯重构不改行为）。
