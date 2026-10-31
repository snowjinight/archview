//! ui 模块单元测试。

use super::App;
use egui::Widget as _;

/// 磁盘预检的 LRU 换出量计算：缓存未满不换出；超限部分换出但
/// 不超过缓存本身占用（盘上就这么多可删）
#[test]
fn disk_evictable_accounting() {
    const G: u64 = 1 << 30;
    // 缓存远未满（1GB/8GB）：无换出，预检退化为纯磁盘剩余判定
    assert_eq!(App::evictable_bytes(G, G, 8 * G), 0);
    // 缓存满额（8GB/8GB）：本批 2GB 把超限 2GB 全部换出腾盘
    assert_eq!(App::evictable_bytes(8 * G, 2 * G, 8 * G), 2 * G);
    // 本批大于整个配额：换出量被缓存占用封顶
    assert_eq!(App::evictable_bytes(G, 8 * G, 8 * G), G);
}

/// Done 兜底补播与"回看提醒"的让位关系。回归：锚点落地落在 keep
/// 生效期（用户回看中）时置顶栏提醒，预取完成（Done）的瞬间兜底
/// 把画面从回看内容拽回锚点、还经 load_extracted 清掉提醒——表现
/// 为"预取一完成按钮直接消失"。钉死两条规则：
/// - 提醒挂起（extract_ready）时 Done 一律不兜底、不清提醒
/// - 无提醒挂起时兜底照常补救（文件在而没播过 → 补播）
#[test]
fn done_fallback_yields_to_extract_ready() {
    let dir = std::env::temp_dir().join(format!("ui_done_fb_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("anchor.jpg");
    std::fs::write(&dest, b"x").unwrap(); // 锚点已落地

    // 手造任务+通道：entry_idx=2 是 keep 改写后的期望目标（回看锚点
    // 场景下 ≠ 实际在播的 0）；无需 archive——session_snapshot 对
    // None 安全返回"无会话"，player 为 None 时 load_extracted 只记账
    let mk_job = |rx: std::sync::mpsc::Receiver<super::ExtMsg>| super::ExtractJob {
        gen: 1,
        entry_idx: 2,
        dest: dest.clone(),
        entry_name: "anchor.jpg".into(),
        total: 0,
        done: 0,
        stage: String::new(),
        pending: std::collections::HashMap::new(),
        batch_len: 1,
        started: std::time::Instant::now(),
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        rx,
    };
    let mut cfg = crate::config::Config::default();
    cfg.cache.dir = dir.join("cache").to_string_lossy().into_owned();

    // 正面：提醒挂起时 Done 让位——不抢画面、不清提醒、任务正常收尾
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(super::ExtMsg::Done).unwrap();
    let mut app = App::new_for_test(cfg.clone());
    app.last_focus = Some(2);
    app.extract_ready = true;
    app.playing = Some(0);
    app.next_gen = 1;
    app.extract = Some(mk_job(rx));
    app.poll_extract();
    assert_eq!(app.playing, Some(0), "提醒挂起时 Done 兜底不得抢画面");
    assert!(app.extract_ready, "提醒应保留待用户消费");
    assert!(app.extract.is_none(), "任务本身应正常收尾");

    // 反面：无提醒挂起时兜底照常补救——把画面切到已落地锚点
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(super::ExtMsg::Done).unwrap();
    let mut app = App::new_for_test(cfg);
    app.last_focus = Some(2);
    app.extract_ready = false;
    app.playing = Some(0);
    app.next_gen = 1;
    app.extract = Some(mk_job(rx));
    app.poll_extract();
    assert_eq!(app.playing, Some(2), "无提醒时兜底应补播已落地锚点");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 目录 = 媒体集合模式（ARCHITECTURE.md 4.2）的打开链路回归：
/// 1) spawn_open 目录分支走真实后台线程（DirVFS::open → stream_cb 注册），
///    Done 消息携带全部媒体条目、VFS 为 FastSeek、无密码概念
/// 2) apply_open_done 状态迁移：stream=true（流式快路径）、视图重建
/// 3) 空目录：打开成功但视图为空 → 统一提示"没有可播放的视频/图片"
#[test]
fn dir_mode_open_pipeline() {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let uniq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ui_dir_mode_{}_{}", std::process::id(), uniq));
    let _ = std::fs::remove_dir_all(&root);
    let sub = root.join("子目录");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(root.join("b.mp4"), b"v").unwrap();
    std::fs::write(sub.join("a.jpg"), b"i").unwrap();
    std::fs::write(root.join("skip.zip"), b"z").unwrap(); // 压缩包跳过
    std::fs::write(root.join("skip.txt"), b"t").unwrap(); // 非媒体跳过

    let mut cfg = crate::config::Config::default();
    cfg.cache.dir = root.join("cache").to_string_lossy().into_owned();
    let mut app = App::new_for_test(cfg);

    // 1) 真实 spawn_open 线程：目录 → DirVFS → Done（Progress 消息越过）
    app.open_path(root.clone());
    let super::OpenJob { rx, .. } = app.open_job.take().expect("open_job 应已建立");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (vfs, entries, id, pw) = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(super::OpenMsg::Progress { .. }) => continue,
            Ok(super::OpenMsg::Done {
                vfs,
                id,
                entries,
                pw,
            }) => break (vfs, entries, id, pw),
            Ok(super::OpenMsg::NeedPw { .. }) => panic!("目录打开不应要求密码"),
            Ok(super::OpenMsg::Failed(e)) => panic!("目录打开失败: {e}"),
            Err(e) => panic!("打开线程消息异常: {e}"),
        }
    };
    assert_eq!(pw, None, "目录集合无密码概念");
    assert_eq!(vfs.access_mode(), super::AccessMode::FastSeek);
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["b.mp4", "子目录/a.jpg"],
        "递归收集媒体（含子目录）且跳过压缩包/非媒体"
    );

    // 2) 状态迁移：stream=true（FastSeek → 流式快路径）、视图含全部媒体
    assert_eq!(
        app.apply_open_done(root.clone(), None, vfs, id, entries, None),
        None
    );
    let a = app.archive.as_ref().unwrap();
    assert!(a.stream, "目录集合应走流式快路径");
    assert_eq!(a.path, root);
    assert_eq!(a.view.len(), 2, "视图应含 2 个媒体条目");
    assert!(app.notice.is_none(), "非空目录不应有提示");

    // 3) 空目录：DirVFS 打开成功但零条目 → 统一提示
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let vfs2: std::sync::Arc<dyn super::ArchiveVFS> =
        std::sync::Arc::new(super::DirVFS::open(&empty, &["mp4".into(), "jpg".into()]).unwrap());
    let entries2 = vfs2.list();
    app.apply_open_done(empty, None, vfs2, 7, entries2, None);
    assert_eq!(app.notice.as_deref(), Some("没有可播放的视频/图片"));

    let _ = std::fs::remove_dir_all(&root);
}

/// 自然序排序管线（rebuild_view 同款：小写键 + alphanumeric_sort::compare_str）。
/// 钉死语义：数字段按数值（1/2/10 而非字典序 1/10/2）、大小写不敏感、
/// 同值前导零 tie-break（少零在前）、CJK+数字混合名按数值排。
/// 只在同脚本组内断言相对顺序，不依赖 crate 的跨脚本比较规则
#[test]
fn natural_sort_pipeline_semantics() {
    fn sort(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| alphanumeric_sort::compare_str(&a.to_lowercase(), &b.to_lowercase()));
        v
    }
    // 纯编号：1/2/9/10/12（字典序会给出 1/10/12/2/9）
    assert_eq!(
        sort(vec!["10.jpg", "2.jpg", "1.jpg", "12.jpg", "9.jpg"]),
        vec!["1.jpg", "2.jpg", "9.jpg", "10.jpg", "12.jpg"]
    );
    // 字符+数字混合 + 大小写不敏感：IMG_2 在 img_10 前
    assert_eq!(
        sort(vec!["img_10.jpg", "IMG_2.jpg"]),
        vec!["IMG_2.jpg", "img_10.jpg"]
    );
    // CJK+数字：第2话 在 第10话 前
    assert_eq!(
        sort(vec!["第10话.jpg", "第2话.jpg"]),
        vec!["第2话.jpg", "第10话.jpg"]
    );
    // 同值前导零 tie-break：2.jpg 在 02.jpg 前（少零在前，结果稳定）
    assert_eq!(sort(vec!["02.jpg", "2.jpg"]), vec!["2.jpg", "02.jpg"]);
}

/// 虚拟化列表（show_viewport 手动行范围）关键行为的回归测试。
///
/// 注意：__run_test_ui 默认装的是空字体（文本不换行、行高全被钳成 18），
/// 必须显式 set_fonts 装真字体，否则测不出"长文本换行破坏等高"类问题
/// （真实应用里 selectable_label 换行曾把行撑成 2 行高，导致渲染范围
/// 按虚高 step 缩水 → 首页底部空隙 + 翻页时行高双稳态来回翻转）。
#[test]
fn virtual_list_scroll_to_row_and_fill() {
    let total = 1000usize;
    let target_row = 500usize;
    // (最终 offset, 实测行高, item_spacing, 视口高, 末行底部, 可视区底部)
    let mut result: Option<(f32, f32, f32, f32, f32, f32)> = None;
    egui::__run_test_ui(|ui| {
        ui.ctx().set_fonts(egui::FontDefinitions::default());
        let spacing = ui.spacing().item_spacing.y;
        let mut row_h: Option<f32> = None;
        for frame in 0..2 {
            let h = row_h.unwrap_or(18.0);
            // 两帧各占一块 200x300 的区域（互不重叠）；200px 宽足够让
            // 不截断的长标签换行——截断修复失效时此测试会变红
            let rect = egui::Rect::from_min_size(
                egui::pos2(0.0, frame as f32 * 310.0),
                egui::vec2(200.0, 300.0),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                let mut measured_h = 0.0f32;
                let mut last_bottom = 0.0f32;
                let out = egui::ScrollArea::vertical()
                    .id_salt("test_list")
                    .auto_shrink([false, false])
                    .animated(false)
                    .show_viewport(ui, |ui, viewport| {
                        let step = h + spacing;
                        ui.set_height(step * total as f32 - spacing);
                        // 与 file_list 相同的手动行范围计算（含 3 行余量）
                        let mut min_row = (((viewport.min.y / step).floor() as isize - 3).max(0)
                            as usize)
                            .min(total);
                        let mut max_row = (viewport.max.y / step).ceil() as usize + 3;
                        if max_row > total {
                            let diff = max_row - min_row;
                            max_row = total;
                            min_row = total.saturating_sub(diff);
                        }
                        let range = min_row..max_row;
                        let y_min = ui.max_rect().top() + min_row as f32 * step;
                        let y_max = ui.max_rect().top() + max_row as f32 * step;
                        let rect =
                            egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);
                        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                            ui.skip_ahead_auto_ids(min_row);
                            if frame == 1 {
                                // 修复后的换算：闭包 ui 的 max_rect.top 是
                                // 内容顶 + min_row*step，行 pos 屏幕坐标 =
                                // top + (pos - range.start) * step
                                let y =
                                    ui.max_rect().top() + (target_row - range.start) as f32 * step;
                                ui.scroll_to_rect(
                                    egui::Rect::from_min_size(
                                        egui::pos2(ui.max_rect().left(), y),
                                        egui::vec2(ui.available_width().max(1.0), h),
                                    ),
                                    Some(egui::Align::Center),
                                );
                            }
                            for i in range.clone() {
                                // 与 file_list 相同的截断渲染：若去掉 .truncate()，
                                // 第 0 行的长标签会换行抬高行高，铺满断言变红
                                let label = if i == 0 {
                                    "这是一个非常长的文件名，宽度超过两百像素，会把行撑成两行高"
                                        .to_string()
                                } else {
                                    format!("[{i}] 短名")
                                };
                                let resp = egui::Button::selectable(i == target_row, label)
                                    .truncate()
                                    .ui(ui);
                                if i == range.start {
                                    measured_h = resp.rect.height();
                                }
                                last_bottom = resp.rect.bottom();
                            }
                        });
                    });
                if frame == 0 {
                    row_h = Some(measured_h); // 行高自校正
                } else {
                    result = Some((
                        out.state.offset.y,
                        measured_h,
                        ui.spacing().item_spacing.y,
                        300.0,
                        last_bottom,
                        out.inner_rect.bottom(),
                    ));
                }
            });
        }
    });
    let (offset, natural_h, spacing, view_h, last_bottom, view_bottom) =
        result.expect("应完成两帧");
    // 说明：测试上下文没有真实字体数据（文本不换行、行高恒 18），
    // "换行破坏等高"无法在此复现，靠上面的 truncate 调用 + 真实应用验证；
    // 此处断言保留坐标换算与铺满行为的覆盖
    assert!(
        natural_h < 26.0,
        "截断后行高应约 18px（实测 {natural_h}），超过则长标签仍在换行"
    );
    // 期望：目标行居中 → offset ≈ 行号×步长 + 行高/2 - 视口/2（±一行内即可）
    let step = natural_h + spacing;
    let expected = target_row as f32 * step + natural_h * 0.5 - view_h * 0.5;
    assert!(
        (offset - expected).abs() < step,
        "滚动 offset={offset} 应接近居中期望 {expected}（step={step}）"
    );
    // 铺满：行高校正后，渲染的最后一行底边应盖到可视区底部（留一行容差）
    assert!(
        view_bottom - last_bottom < step,
        "末行底部 {last_bottom} 距可视区底 {view_bottom} 超过一行，列表未铺满"
    );
}

/// 高亮追踪滚动回归测试（用户报告的 BUG）：
/// 列表已手动滚动到远处（~200 行），此时 playing 切到 51 行触发
/// scroll_to_rect 高亮居中——列表应回到 51 行附近，而不是跳到底部。
///
/// 多帧多 pass 模拟真实时序（滚轮事件 → 动画收敛），区别于上面的
/// 单 pass 两帧测试：这里 ScrollArea 是默认 animated（应用未关闭动画）。
#[test]
fn virtual_list_track_scroll_from_manually_scrolled_position() {
    let total = 10_000usize;
    let target = 51usize;
    let ctx = egui::Context::default();
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(400.0, 300.0),
    ));
    let mut row_h: Option<f32> = None;
    let mut offsets: Vec<f32> = Vec::new();
    let mut scroll_to = false;

    let mut t = 0.0;
    for frame in 0..160 {
        t += 0.016;
        raw.time = Some(t);
        if (5..45).contains(&frame) {
            // 鼠标悬停列表，滚轮向下翻（看后面的行）
            raw.events
                .push(egui::Event::PointerMoved(egui::pos2(100.0, 150.0)));
            raw.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, -8.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            });
        } else if frame >= 45 {
            // 鼠标移到列表外（模拟回到视频区滚轮翻页；列表不 hover，
            // 滚轮惯性尾巴才不会每帧清掉 scroll_to_rect 的动画目标）
            raw.events
                .push(egui::Event::PointerMoved(egui::pos2(2000.0, 150.0)));
        }
        if frame == 50 {
            // 模拟 playing 变化触发高亮滚动（视频区滚轮翻页 → 解压完成 → playing 更新）
            scroll_to = true;
        }
        let output = ctx.run_ui(raw.take(), |ui| {
            let h = row_h.unwrap_or(18.0);
            let spacing = ui.spacing().item_spacing.y;
            let step = h + spacing;
            let mut measured = 0.0f32;
            let out = egui::ScrollArea::vertical()
                .id_salt("track_list")
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, viewport| {
                    ui.set_height(step * total as f32 - spacing);
                    let mut min_row =
                        (((viewport.min.y / step).floor() as isize - 3).max(0) as usize).min(total);
                    let mut max_row = (viewport.max.y / step).ceil() as usize + 3;
                    if max_row > total {
                        let diff = max_row - min_row;
                        max_row = total;
                        min_row = total.saturating_sub(diff);
                    }
                    let range = min_row..max_row;
                    let y_min = ui.max_rect().top() + min_row as f32 * step;
                    let y_max = ui.max_rect().top() + max_row as f32 * step;
                    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);
                    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                        ui.skip_ahead_auto_ids(min_row);
                        if scroll_to {
                            // 与 file_list 相同的换算（isize 防下溢 wrap）
                            let y = ui.max_rect().top()
                                + (target as isize - range.start as isize) as f32 * step;
                            ui.scroll_to_rect(
                                egui::Rect::from_min_size(
                                    egui::pos2(ui.max_rect().left(), y),
                                    egui::vec2(ui.available_width().max(1.0), h),
                                ),
                                Some(egui::Align::Center),
                            );
                        }
                        for i in range.clone() {
                            let resp = egui::Button::selectable(i == target, format!("[{i}] 短名"))
                                .truncate()
                                .ui(ui);
                            if i == min_row {
                                measured = resp.rect.height();
                            }
                        }
                    });
                });
            if row_h.is_none() && measured > 0.0 {
                row_h = Some(measured);
            }
            offsets.push(out.state.offset.y);
        });
        output.drop_without_applying_deltas();
        scroll_to = false;
    }

    let step = row_h.unwrap_or(18.0) + 3.0; // item_spacing.y 默认 3
    let before = offsets[49]; // 触发滚动前
    let final_offset = *offsets.last().unwrap();
    assert!(
        before > 100.0 * step,
        "前置条件不满足：滚轮后 offset={before} 应已滚到 100 行以外"
    );
    let expected = target as f32 * step + row_h.unwrap_or(18.0) * 0.5 - 150.0;
    assert!(
        (final_offset - expected).abs() < 5.0 * step,
        "高亮追踪失败：最终 offset={final_offset}，期望 ≈{expected}（target 居中），\
         触发前 offset={before}，step={step}，轨迹尾部={:?}",
        &offsets[140..]
    );
}

/// 滑动窗口冒烟测试（UI 层端到端）：模拟用户在批内来回跳动，验证
/// 窗口随 focus 滑动、解码流不重启、缓存命中即时播放。
///
/// 造包：6 × 1MB 伪随机 jpg 压进同一 folder（solid）；
/// 下游预取 3MB（窗口 = 目标 + 2 个下游条目，恰好 3MB）、上游预取
/// 关闭（聚焦下游滑动）。跳动链 0 → 1 → 0 → 2 → 1 → 0 →（等解净）
/// → 5 → 0，逐步断言：
/// - 批内切换（步骤 2）：窗口 [0,1,2]→[1,2,3] 平移（0 出窗、3 进窗），
///   cancel 句柄不变（解码线程不重启）、started 不重置、画面停在旧目标
/// - 回跳缓存命中（步骤 3）：点击项在锚点上游 keep——立即播放但会话
///   纹丝不动（窗口/focus/gen/pending 全保持，后台续解不受回看干扰）
/// - 向下游点击（步骤 4）：未落地 → 重定向滑窗 [2,3,4]、锚点向下游
///   推进、gen 自增
/// - keep 连击（步骤 4b）：回看中连点 1→0 仍全 keep——绝不像旧 focus
///   踩踏那样"点完 10 再点 11 触发窗口重建"
/// - 空闲后批外跳（步骤 6）：会话已退出 → 重 spawn（新 cancel）
/// - 整窗已落地（步骤 7）：不重定向会话，直接播放
/// - 终态：全部条目落地且内容无误、无错误、会话干净退出
///
/// 时序说明：断言点都紧跟在目标落地之后（μs 级），此时解码线程正
/// 在解下一个 1MB 条目（百 ms 级），"未落地"状态窗口期充足，不抖
#[test]
fn sliding_window_smoke_in_batch_back_and_forth() {
    use crate::vfs::sevenz_vfs::SevenzVFS;
    use crate::vfs::{ArchiveVFS, SessionState};
    use std::sync::Arc;

    // xorshift64 伪随机数据：内容互异，落地后逐字节比对可捕获错位
    fn prng_vec(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed | 1;
        let mut v = Vec::with_capacity(len);
        while v.len() < len {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            v.extend_from_slice(&s.to_le_bytes());
        }
        v.truncate(len);
        v
    }

    let dir = std::env::temp_dir().join(format!("ui_slide_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let srcs: Vec<(String, Vec<u8>)> = (0..6)
        .map(|i| {
            (
                format!("p{i:02}.jpg"),
                prng_vec(i as u64 + 0x9E37, 1024 * 1024),
            )
        })
        .collect();

    // solid 7z：全部条目压进同一 folder（顺序解压流，滑动窗口的主战场）
    let arch = dir.join("slide.7z");
    {
        let mut paths = Vec::new();
        for (name, data) in &srcs {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            paths.push(p);
        }
        let mut w = sevenz_rust::SevenZWriter::create(&arch).unwrap();
        let entries: Vec<sevenz_rust::SevenZArchiveEntry> = paths
            .iter()
            .map(|p| {
                sevenz_rust::SevenZArchiveEntry::from_path(
                    p,
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let readers: Vec<sevenz_rust::SourceReader<std::fs::File>> = paths
            .iter()
            .map(|p| sevenz_rust::SourceReader::new(std::fs::File::open(p).unwrap()))
            .collect();
        w.push_archive_entries(entries, sevenz_rust::SeqReader::new(readers))
            .unwrap();
        w.finish().unwrap();
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    }

    // 独立缓存目录 + 窗口参数：下游预取 3MB → 窗口恰为 [目标, +1, +2]
    let mut cfg = crate::config::Config::default();
    cfg.cache.dir = dir.join("cache").to_string_lossy().into_owned();
    cfg.cache.max_size_mb = 4096;
    cfg.extract.prefetch_downstream_mb = 3;
    cfg.extract.prefetch_upstream_mb = 0;
    let mut app = super::App::new_for_test(cfg);

    let vfs: Arc<dyn ArchiveVFS> = Arc::new(SevenzVFS::open(&arch, None).unwrap());
    let entries = vfs.list();
    assert_eq!(entries.len(), 6);
    let session: crate::vfs::SharedSession = Arc::new(std::sync::Mutex::new(SessionState {
        targets: Default::default(),
        focus: None,
        alive: false,
        gen: 0,
    }));
    app.archive = Some(super::ArchiveState {
        id: 7,
        name: "slide.7z".into(),
        path: arch.clone(),
        password: None,
        entries,
        view: (0..6).collect(),
        vfs,
        stream: false,
        session,
    });

    // 会话当前窗口（下标升序）
    let window = |app: &super::App| -> Vec<usize> {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        let mut v: Vec<usize> = st.targets.values().map(|(i, _)| *i).collect();
        v.sort_unstable();
        v
    };
    // 模拟 UI 主循环：轮询消息直到目标开播（60s 兜底防死等）
    fn wait_playing(app: &mut super::App, idx: usize, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while app.playing != Some(idx) {
            if std::time::Instant::now() > deadline {
                panic!("等待 {what}（entry {idx}）播放超时");
            }
            if let Some(e) = &app.error {
                panic!("等待 {what} 时出错: {e}");
            }
            app.poll_extract();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    // 等条目落地出窗（targets 移除）：消除断言窗口期里"落地恰好
    // 把条目移出 pending"与"点击动没动窗口"两种效应的混淆
    fn wait_pending_drop(app: &mut super::App, idx: usize, what: &str) {
        let in_window = |app: &super::App| {
            app.archive
                .as_ref()
                .unwrap()
                .session
                .lock()
                .unwrap()
                .targets
                .values()
                .any(|(i, _)| *i == idx)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while in_window(app) {
            if std::time::Instant::now() > deadline {
                panic!("{what}超时：entry {idx} 仍在窗口");
            }
            if let Some(e) = &app.error {
                panic!("{what}时出错: {e}");
            }
            app.poll_extract();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    // ---- 步骤 1：批外首发（spawn 解码线程，首窗 [0,1,2]）----
    app.start_extract(0);
    let j = app.extract.as_ref().expect("首发应有解压任务");
    let cancel1 = j.cancel.clone();
    let started1 = j.started;
    assert_eq!(j.entry_idx, 0);
    assert_eq!(j.batch_len, 3);
    assert_eq!(window(&app), vec![0, 1, 2], "首窗应为 [0,1,2]");
    {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        assert_eq!(st.focus, Some(0));
        assert!(st.alive, "线程启动后 alive 应为 true");
    }
    wait_playing(&mut app, 0, "p00 首播");

    // ---- 步骤 2：批内跳动 0→1（解码流续跑，窗口平移 [0,1,2]→[1,2,3]）----
    app.start_extract(1);
    let j = app.extract.as_ref().expect("批内切换后任务应仍在");
    assert_eq!(j.entry_idx, 1);
    assert_eq!(j.batch_len, 3);
    assert_eq!(app.next_gen, 2);
    assert!(
        Arc::ptr_eq(&j.cancel, &cancel1),
        "批内跳动不应重启解码线程（cancel 句柄应复用）"
    );
    assert_eq!(
        j.started, started1,
        "批内切换（was_pending）不应重置覆盖层宽限起点"
    );
    assert_eq!(
        window(&app),
        vec![1, 2, 3],
        "窗口应滑动：0 已落地出窗、3 进窗"
    );
    {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        assert_eq!(st.focus, Some(1));
        assert!(st.alive, "解码线程应仍在跑");
    }
    assert_eq!(app.playing, Some(0), "新目标落地前画面应停在旧目标");
    wait_playing(&mut app, 1, "批内切换后 p01 落地自动播放");

    // ---- 步骤 3：回跳 1→0（缓存命中 + 点击项在锚点上游 → keep：会话纹丝不动）----
    let w3 = window(&app); // 快照（= 步骤 2 窗口减已落地项，如 {2,3}）
    let pend3 = app.extract.as_ref().unwrap().pending.clone();
    app.start_extract(0);
    assert_eq!(app.playing, Some(0), "keep 缓存命中应立即播放");
    assert_eq!(window(&app), w3, "keep 不碰会话：窗口保持不动");
    {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        assert_eq!(st.focus, Some(1), "锚点只向下游推进：keep 不改 focus");
        assert!(st.alive, "解码线程照常在跑原计划");
    }
    assert_eq!(app.next_gen, 2, "keep 不重定向（gen 不自增）");
    {
        let j = app.extract.as_ref().expect("keep 应保有原任务");
        assert_eq!(j.entry_idx, 0, "j 目标切到点击项（Done 兜底以它为准）");
        assert_eq!(j.pending, pend3, "pending 不重建（后台剩余量如实）");
        assert!(Arc::ptr_eq(&j.cancel, &cancel1), "keep 不重启解码线程");
    }
    app.start_extract(0); // 再点同项：去重早退，会话仍不动
    assert_eq!(window(&app), w3);
    assert_eq!(app.next_gen, 2);

    // ---- 步骤 4：向下游点击 2（未落地 → 重定向滑窗 [2,3,4]、锚点推进）----
    app.start_extract(2);
    assert_eq!(window(&app), vec![2, 3, 4], "点击未落地项应重定向滑窗");
    assert_eq!(app.playing, Some(0), "2 落地前画面不动");
    assert_eq!(app.next_gen, 3, "重定向自增 gen");
    {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        assert_eq!(st.focus, Some(2), "锚点随向下游点击推进");
    }
    assert!(Arc::ptr_eq(&app.extract.as_ref().unwrap().cancel, &cancel1));

    // ---- 步骤 4b：keep 连击（用户场景：回看中连点多个缓存项）----
    // 0/1 均已落地且在锚点 focus=2 上游：点 1 → keep，紧接着再点 0 →
    // 仍 keep——绝不像旧 focus 踩踏那样"点完 10 再点 11 触发窗口重建"
    // 先等 2 落地出窗（消除断言窗口期里"落地恰把 2 移出 pending"的
    // 竞态——keep 断言要证的是"点击不动窗口"，不能混入落地效应）
    wait_pending_drop(&mut app, 2, "4b 前等待 p02 落地出窗");
    let w4 = window(&app); // 快照（此刻窗内仅剩未落地项）
    app.start_extract(1);
    assert_eq!(app.playing, Some(1), "keep 应立即播放");
    assert_eq!(window(&app), w4, "连点缓存项不应动窗口");
    assert_eq!(app.next_gen, 3, "keep 不自增 gen");
    {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        assert_eq!(st.focus, Some(2), "连点缓存项不让锚点回退上游");
    }
    app.start_extract(0);
    assert_eq!(app.playing, Some(0));
    assert_eq!(window(&app), w4);
    assert_eq!(app.next_gen, 3);

    // ---- 步骤 5：等整窗解净（会话空闲退出）----
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        app.poll_extract();
        let drained = {
            let st = app.archive.as_ref().unwrap().session.lock().unwrap();
            app.extract.is_none() && !st.alive && st.targets.is_empty()
        };
        if drained {
            break;
        }
        assert!(std::time::Instant::now() <= deadline, "等待窗口解净超时");
        if let Some(e) = &app.error {
            panic!("解净阶段出错: {e}");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // keep 后 j.entry_idx 停在 0 且 playing==Some(0)：FileDone 不切画面、
    // Done 兜底补播（playing != Some(entry_idx)）也不触发——keep 的安全性
    assert_eq!(app.playing, Some(0), "解净后画面应停在回看项 p00");
    // 锚点 2 落地时用户在回看（自动播放被吞）→ 应置"解压完成待消费"提醒
    assert!(
        app.extract_ready,
        "回看中锚点落地应置解压完成提醒（顶栏提醒的依据）"
    );

    // ---- 步骤 5b：会话退出后按 last_focus 跳回锚点（"回到锚点"的
    // 可测等价路径）----
    // 空闲退出只清 targets/alive，focus 与 last_focus 保留——用户回看
    // 缓存后一键跳回解压推进点（2）。锚点已落地 → 立即播放；下游 5
    // 尚未落地 → 重算预取窗口 [5] 并重 spawn 会话顺带补齐（秒开锚点
    // + 预取两不误）
    assert_eq!(
        app.last_focus,
        Some(2),
        "会话退出后 last_focus 应保留（回到锚点的依据）"
    );
    assert_eq!(
        app.archive.as_ref().unwrap().session.lock().unwrap().focus,
        Some(2),
        "会话空闲退出后 focus 锚点应保留"
    );
    app.start_extract(2);
    let cancel2 = app
        .extract
        .as_ref()
        .expect("5b 应重 spawn 预取任务")
        .cancel
        .clone();
    assert!(
        !Arc::ptr_eq(&cancel2, &cancel1),
        "会话空闲退出后回锚点应重起线程（新 cancel）"
    );
    assert_eq!(app.playing, Some(2), "锚点已落地应立即播放");
    assert!(!app.extract_ready, "跳回锚点播放后提醒应清除");
    assert_eq!(window(&app), vec![5], "锚点下游未落地项应进预取窗口");
    assert_eq!(app.next_gen, 4, "重 spawn 会话自增 gen");
    {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        assert_eq!(st.focus, Some(2), "回到锚点不改锚点");
        assert!(st.alive, "预取会话应重新拉起");
    }
    assert_eq!(app.last_focus, Some(2), "回到锚点后 last_focus 保持");
    // 等预取会话（补解 5）收尾，还原"会话已退出"前提给步骤 6
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        app.poll_extract();
        let drained = {
            let st = app.archive.as_ref().unwrap().session.lock().unwrap();
            app.extract.is_none() && !st.alive && st.targets.is_empty()
        };
        if drained {
            break;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "等待预取会话收尾超时"
        );
        if let Some(e) = &app.error {
            panic!("预取收尾阶段出错: {e}");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // 预取会话补解的 5 应已落地（内容在终态统一比对）；playing 停在
    // 锚点 2（FileDone 的 i=5 ≠ j.entry_idx=2，不切画面——正确）
    let dest5 = crate::cache::dest_path(&dir.join("cache"), 7, 5, &srcs[5].0);
    assert_eq!(
        std::fs::metadata(&dest5).unwrap().len(),
        srcs[5].1.len() as u64,
        "预取会话应把下游 5 解压落地"
    );
    assert_eq!(app.playing, Some(2), "预取完成画面应停在锚点");

    // ---- 步骤 5c：死后翻看已缓存项（"缓存命中预取会话启动"分支）——
    // 锚点不被污染 ----
    // 删掉 5 的落地文件模拟"未落地的下游"，再点已缓存的 3：目标秒开 +
    // 预取重定向补解 5。缓存命中不算解压推进 → last_focus 保持 2，
    // "回到锚点"仍指向真正的解压位置
    std::fs::remove_file(&dest5).unwrap();
    app.start_extract(3);
    assert_eq!(app.playing, Some(3), "已缓存目标应立即播放");
    assert_eq!(window(&app), vec![5], "缺失的下游 5 应进预取窗口");
    assert_eq!(app.next_gen, 5, "预取重定向自增 gen");
    assert_eq!(
        app.last_focus,
        Some(2),
        "缓存命中不应推进 last_focus（回看不污染锚点）"
    );
    assert!(!app.extract_ready, "缓存命中不应置位提醒");
    // 等预取会话（补解 5）收尾，还原全落地前提给步骤 7
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        app.poll_extract();
        let drained = {
            let st = app.archive.as_ref().unwrap().session.lock().unwrap();
            app.extract.is_none() && !st.alive && st.targets.is_empty()
        };
        if drained {
            break;
        }
        assert!(std::time::Instant::now() <= deadline, "5c 预取收尾超时");
        if let Some(e) = &app.error {
            panic!("5c 预取收尾阶段出错: {e}");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    // ---- 步骤 7：再点 0（整窗已落地：不重定向会话，直接播放）----
    app.start_extract(0);
    assert_eq!(app.playing, Some(0), "整窗已落地应直接播放");
    assert!(
        window(&app).is_empty(),
        "整窗已落地不应重定向（空 targets 会误杀解码线程）"
    );

    // ---- 终态：全部落地 + 内容无误 + 会话干净退出 ----
    for (i, (name, expect)) in srcs.iter().enumerate() {
        let dest = crate::cache::dest_path(&dir.join("cache"), 7, i, name);
        let data = std::fs::read(&dest).unwrap_or_else(|e| panic!("条目 {name} 应落地: {e}"));
        assert_eq!(data, *expect, "条目 {name} 内容错位");
    }
    {
        let st = app.archive.as_ref().unwrap().session.lock().unwrap();
        assert!(!st.alive, "终态会话应已退出");
        assert!(st.targets.is_empty(), "终态 targets 应为空");
    }
    assert!(app.error.is_none(), "全程不应有错误");
    let _ = std::fs::remove_dir_all(&dir);
}
