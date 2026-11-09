> [返回目录](README.md)

## 10. encode 三壳接入（CLI·SendTo / UI，2026-11-02）

引擎（[§9](09_encode引擎e2e攻坚.md)）之上按 [ARCHITECTURE §15.5](../架构设计/15_encode引擎.md) 接三壳：`[encode]` config 节、
`archview encode` CLI 子命令 + SendTo 第三项、UI 多选/右键/确认窗/后台
任务。壳侧排掉六个坑（前五个是编译期即报的 egui 0.36 / Rust 实测差异，
第六个是纯逻辑竞态）：

**坑 1：`Event::Key` 模式必须带 `..`（E0027）**。0.36 的 Key 事件有
`physical_key` 等字段，模式逐字段写会报"missing field"。事件匹配一律
`Event::Key { key, pressed, repeat, modifiers, .. }`。

**坑 2：`Modifiers.command` 是字段不是方法（E0599）**。`modifiers.command()`
报 "field, not method"——直接 `modifiers.command`（bool）。编译器提示的
`command_only()` 语义不同（仅 Ctrl 且无其它修饰），不要顺手采纳。

**坑 3：`Ui` 没有 `close_menu()`（E0599）**。`Response::context_menu`
内点 Button 菜单**自动关闭**，无需也不能显式调用。

**坑 4：`move ||` 闭包内 `&out` 夺走所有权（E0382）**。move 闭包先夺取
`out` 再借用，闭包外 EncodeJob 构造再用 `out` 即"moved value"。修法
`let out_t = out.clone();` 且**闭包体内改用 `&out_t`**——只加 clone 不改
闭包体引用，错误原样复现（本次实际踩了两轮）。

**坑 5：`Response::on_hover_cursor` 消耗 self（E0382）**。0.36 该方法
`fn on_hover_cursor(self, ...) -> Response` 取所有权，之后还要 `resp.clicked()`
就得 `resp.clone().on_hover_cursor(...)`（Response 是廉价 Arc clone）。

**坑 6：可操作绿条 vs 帧首 any_click 清除的竞态（纯逻辑）**。"完成绿条
点击打开输出目录"的朴素实现会静默失效：`clear_notice_on_click` 在帧首
跑（update 开头），任意点击即清 notice——同一帧稍后渲染绿条时 notice
已是 None，Label 的 `clicked()` 永远收不到那次点击。修法：`encode_done_dir`
在场（动作未消费）时帧首**不清**，由绿条控件自身消费点击（开目录 +
take 动作 + 消条）；新消息经 `set_notice`/`set_err` 统一清伴随动作，
杜绝旧动作挂在新消息上。语义代价：可操作绿条不被无关点击消除——可接受
（它是待执行动作，不是纯提示）。

**UI 多选语义决定**（apply_row_click，钉死在测试里）：普通点击=单选+锚点
+播放；Ctrl=toggle 且**锚点跟随被点击行**（含移除时——与资源管理器一致，
测试初版想当然以为锚点停在原选中行而挂掉）；Shift=从锚点整段替换（含反
向）；锚点是代际残留（切包后不在行缓存）退化单行不 panic；切包
（apply_open_done）清 selected+锚点（entry 下标对新包是错位引用）。

**多选视觉与播放语义修订**（用户反馈两条，[§11](11_修饰键两条数据路径.md) 修复后的第二轮打磨；视觉
方案又经用户实测打回、第三轮定稿）：
- 选中视觉两试皆废，根因**不是**初判的"撞色"——egui 0.36
  widgets/button.rs 渲染守卫：`has_frame_margin && (state != Inactive ||
  frame_when_inactive)` 不满足时 layout.frame 被剥成无填充
  `Frame::new().inner_margin(...)`，**显式 fill 根本不进渲染**。而
  `Button::selectable(selected)` 内部是 `.selected(selected).
  frame_when_inactive(selected)`——非播放行 selected=false → Inactive 态
  fill 必被剥：初版 fill(selection.bg_fill) 从未生效过（用户"看不出选中"
  的真因）；第二版琥珀 fill 同理——仅 hover/按住瞬间 state=Active 可见
  （用户实测"点住变黄、松手恢复"）；而播放行 selected=true →
  frame_when_inactive=true → frame 恒生效，琥珀覆盖主题蓝（"正在播放变
  黄"）。调 frame_when_inactive(true) 是死路——全员常驻按钮底色。终版：
  大写 S 前缀 + 主题感知琥珀着色（AMBER_DARK/LIGHT 常量，与 hover 气泡
  目录段共用——同色相视觉语言）。文字显式色无条件优先（widget_text.rs
  `text_color.unwrap_or_else(...)`），不受背景 fill 守卫影响；元组
  IntoAtoms 拼 S 段+文件名段，S 不放大字号（AtomLayout 行高取各段
  max，放大破坏等高前提）、无真加粗可选（默认字体无 bold variant，
  strong() 只是提亮色标志且被显式色覆盖）。演进插曲：✓（fallback 字体
  度量风险）→[x]→[s] 纯文本版先后落地，用户去 [] 求更醒目——大写 S +
  琥珀定稿。playing 行仍由 selectable 选中态着主题蓝（S 保持琥珀，暖
  冷对比清晰）。
- Ctrl/Shift 点击**恒不播放**（原实现无锚点 Shift 等同普通点击会播放）：
  多选通常为批量编码服务，点一下就切歌反而打断当前播放。无锚点 Shift =
  单选+锚点落地，下续 Shift 才有范围基准。测试以「play 开头清
  error/notice」为无 archive 环境下的 play 调用观测点。

132 passed（基线 113）：CLI 11 测（parse/收集/唯一族/自然序/缺口警告/
输出命名/选项组装/密码合并），UI 3 测（多选语义含不播放断言/切包清空/
click_modifiers 快照），config 防漂移等 5 测。
