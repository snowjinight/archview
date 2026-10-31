//! 播放与后台解压链路：mpv 播放器管理、批次计算与磁盘预检、solid 会话解压任务提交/轮询、密码重放与已解压文件装载。

use super::*;

impl App {
    // ---- 播放 ----

    /// 确保 mpv 播放器就绪；返回 false 时已设置错误。
    /// GL/egui 上下文来自启动期 CreationContext（照搬开源参考方案：
    /// PlayerService 惰性创建，GL 句柄启动时即注入备用）
    pub(super) fn ensure_player(&mut self) -> bool {
        if self.player.is_none() {
            let Some(gl) = self.gl.as_deref() else {
                self.set_err("glow 上下文不可用（应使用 eframe glow 后端启动）");
                return false;
            };
            match PlayerService::new(&self.cfg, gl, &self.egui_ctx) {
                Ok(p) => self.player = Some(p),
                Err(e) => {
                    self.set_err(e);
                    return false;
                }
            }
        }
        true
    }

    pub(super) fn play(&mut self, entry_idx: usize) {
        // 先读模式与条目标记结束借用（后续各分支需要 &mut self）
        let Some((stream, encrypted, has_pw, name)) = self.archive.as_ref().map(|a| {
            (
                a.stream,
                a.entries[entry_idx].encrypted,
                a.password.is_some(),
                a.entries[entry_idx].name.clone(),
            )
        }) else {
            return;
        };
        // 内容加密条目（zip AES/ZipCrypto；7z/RAR 加密头已挡在打开阶段）：
        // 无密码先弹密码框，带密码重建 VFS 后再播——图片/视频同规则
        if encrypted && !has_pw {
            self.pw_prompt = Some(PwPrompt {
                kind: PwKind::Play(entry_idx),
                input: String::new(),
                wrong: false,
            });
            return;
        }
        // 图片路由：流式 zip 图片（静态图含全部 webp + gif 动图，13.4 第 4 步）
        // 走纹理直显（跳过 mpv，不解压）；7z/RAR 图片仍走解压会话（批次/预取
        // 机制复用），落地后在 load_extracted 分流为纹理解码
        if stream && crate::imgview::wants_texture(&self.cfg, &name) {
            self.play_image(entry_idx);
            return;
        }
        // 视频路径需要播放器，先就绪
        if !self.ensure_player() {
            return;
        }
        if stream {
            // 快路径：流式播放（zvf://）
            let Some(arch) = &self.archive else { return };
            let archive_id = arch.id;
            if let Some(p) = &self.player {
                p.load(archive_id, entry_idx);
            }
            // 切视频清防闪快照与图片源：否则快照停留在视频前的旧图，下次
            // 切图解码间隙会闪现那张隔了好几条的图
            self.hold_img = None;
            self.img_source = None;
            self.playing = Some(entry_idx);
            self.dragging = false;
            tracing::info!("播放: {name}");
        } else {
            // 慢路径：后台解压（7z/RAR 固态；RAR 运行时才暴露的加密走线程反馈）
            self.start_extract(entry_idx);
        }
    }

    /// 图片纹理直显（流式包静态图含全部 webp + gif 动图）：停掉 mpv 画面，
    /// 请求后台解码（LRU 命中直接显示）。解码失败在 poll_img 中回退 mpv 兜底
    fn play_image(&mut self, entry_idx: usize) {
        let Some((key, source, name)) = self.archive.as_ref().map(|a| {
            (
                (a.id, entry_idx),
                ImageSource::Stream(a.vfs.clone(), a.entries[entry_idx].clone()),
                a.entries[entry_idx].name.clone(),
            )
        }) else {
            return;
        };
        // 曾经解码失败回退 mpv：直接走兜底装载，不再重复解码
        if self.img_fallback.contains(&key) {
            self.playing = Some(entry_idx);
            self.img_view = ImgView::default();
            self.img_fallback_mpv(entry_idx);
            return;
        }
        // mpv 若有画面（上一张为视频/动图），stop 清画面断音频
        if let Some(p) = &self.player {
            p.stop();
        }
        let max_dim = self.cfg.ui.image_max_dim;
        // 记录解码源：放大到 1:1 附近时按需再解原图（image_view 触发）
        self.img_source = Some((key, source.clone(), name.clone()));
        if !self.img.has(key) && !self.img.is_pending(key) {
            self.img.request(key, source, max_dim, &name);
        }
        self.playing = Some(entry_idx);
        self.dragging = false;
        // 切图即复位视图（重新适应窗口）
        self.img_view = ImgView::default();
        tracing::info!("图片直显: {name}");
        self.prefetch_images();
    }

    /// 当前纹理图片的 key (archive_id, playing)。无包/未播放/当前条目
    /// 非纹理图（视频、开关关闭、曾解码失败回退 mpv）= None。nav/panels
    /// 判定图片模式与 poll_img 匹配失败回退共用
    pub(super) fn tex_image_key(&self) -> Option<ImgKey> {
        let arch = self.archive.as_ref()?;
        let i = self.playing?;
        let key = (arch.id, i);
        // 回退过的条目不再走纹理：否则 mpv 画面被 image_view 的
        // "正在解码图片…"占位永久遮挡（mpv 分支要求 img_key.is_none()）
        (!self.img_fallback.contains(&key)
            && crate::imgview::wants_texture(&self.cfg, &arch.entries[i].name))
        .then_some(key)
    }

    /// 每帧轮询图片解码结果：Ready 纹理已由服务上传进 LRU（绘制时查缓存，
    /// 无需额外处理）；Failed 清 pending，当前显示的图回退 mpv 兜底播放，
    /// 预取图只记日志（用户翻到时 mpv 不一定可用，见 img_fallback_mpv）
    pub(super) fn poll_img(&mut self) {
        for ev in self.img.poll() {
            match ev {
                ImgEvent::Ready(key) => tracing::debug!("图片纹理就绪: {key:?}"),
                ImgEvent::Failed(key, e) => {
                    if self.tex_image_key() == Some(key) {
                        tracing::warn!("图片解码失败，回退 mpv 播放: {e}");
                        self.img_fallback_mpv(key.1);
                    } else {
                        tracing::warn!("图片预取解码失败: {e}");
                    }
                }
            }
        }
        self.heal_current_image();
    }

    /// 每帧自愈：当前应为纹理图，却既不在缓存也不在解码中 → 立即补发请求。
    /// 此前请求只由 nav 的 play_image 发起：条目若在他图显示/占位期间被
    /// 预取推入挤出 LRU（占位期 shown 保护的还是上一张图），回到该图必须
    /// 再翻动一次才恢复——用户停在这张图时就表现为永久"正在解码图片…"
    /// 占位符。补发幂等（已缓存/解码中直接返回），任何掉出一帧内重建；
    /// 与看门狗配合：pending 卡死 10s 剔除后也走这里重新发起
    fn heal_current_image(&mut self) {
        let Some(key) = self.tex_image_key() else {
            return;
        };
        if self.img.has(key) || self.img.is_pending(key) {
            return;
        }
        let Some(arch) = self.archive.as_ref() else {
            return;
        };
        let e = &arch.entries[key.1];
        let src = ImageSource::Stream(arch.vfs.clone(), e.clone());
        let name = e.name.clone();
        let max_dim = self.cfg.ui.image_max_dim;
        tracing::warn!("当前图不在缓存且无在途解码，补发请求: {name}");
        self.img_source = Some((key, src.clone(), name.clone()));
        self.img.request(key, src, max_dim, &name);
    }

    /// 图片解码失败的 mpv 兜底：mpv 支持的图片格式更广。流式包直接 load
    /// 流 URL；7z/RAR 图片通常已解压落盘，load_path 缓存文件。
    /// 记入 img_fallback：tex_image_key 剔除该条目，mpv 画面才能显示；
    /// 再次播放同条目时直接走 mpv 不重复解码
    fn img_fallback_mpv(&mut self, entry_idx: usize) {
        if !self.ensure_player() {
            return;
        }
        let Some((stream, archive_id, name)) = self
            .archive
            .as_ref()
            .map(|a| (a.stream, a.id, a.entries[entry_idx].name.clone()))
        else {
            return;
        };
        self.img_fallback.insert((archive_id, entry_idx));
        // 回退 mpv 同切视频：清快照防隔图闪现
        self.hold_img = None;
        self.img_source = None;
        if let Some(p) = &self.player {
            if stream {
                p.load(archive_id, entry_idx);
            } else {
                let dir = cache::cache_dir(&self.cfg);
                let dest = cache::dest_path(&dir, archive_id, entry_idx, &name);
                if dest.exists() {
                    p.load_path(&dest);
                }
            }
        }
        self.dragging = false;
    }

    /// 图片 ±1 预取：view 中相邻条目里的图片（含 gif 动图）请求后台解码，翻页零
    /// 等待（LRU 上限自然淘汰）。7z/RAR 仅对已完整落盘的条目发起——未落盘的
    /// 归 extract 会话管（半截文件也解不出图）
    fn prefetch_images(&mut self) {
        // 阶段一（不可变借用）：收集邻居候选（位置 → key + 解码源 + 名）
        let cands: Vec<(ImgKey, ImageSource, String)> = {
            let Some(arch) = self.archive.as_ref() else {
                return;
            };
            let Some(cur) = self.playing else { return };
            let Some(pos) = arch.view.iter().position(|&i| i == cur) else {
                return;
            };
            let dir = cache::cache_dir(&self.cfg);
            let mut v = Vec::with_capacity(2);
            for p in [pos.checked_sub(1), Some(pos + 1)].into_iter().flatten() {
                let Some(&i) = arch.view.get(p) else { continue };
                let e = &arch.entries[i];
                if !crate::imgview::wants_texture(&self.cfg, &e.name) {
                    continue;
                }
                let src = if arch.stream {
                    ImageSource::Stream(arch.vfs.clone(), e.clone())
                } else {
                    let dest = cache::dest_path(&dir, arch.id, i, &e.name);
                    if std::fs::metadata(&dest)
                        .map(|m| m.len() == e.size)
                        .unwrap_or(false)
                    {
                        ImageSource::File(dest)
                    } else {
                        continue;
                    }
                };
                v.push(((arch.id, i), src, e.name.clone()));
            }
            v
        };
        // 阶段二（可变借用）：已缓存/解码中的跳过（has 不打乱 LRU）；
        // 曾回退 mpv 的条目跳过（mpv 链路显示，不再纹理解码）
        let max_dim = self.cfg.ui.image_max_dim;
        for (key, src, name) in cands {
            if self.img_fallback.contains(&key) {
                continue;
            }
            if !self.img.has(key) && !self.img.is_pending(key) {
                self.img.request(key, src, max_dim, &name);
            }
        }
    }

    /// 组批 = 会话窗口：目标 + view 顺序（显示顺序）下游条目
    /// （prefetch_downstream_mb 封顶）+ 定位免费区上游条目
    /// （prefetch_upstream_mb 封顶）。已落地条目跳过
    /// （无需解压、不占批额度；目标本身已缓存时窗口纯做预取）。返回
    /// (窗口条目下标, 窗口总字节)。窗口随 focus 滑动：每次目标切换都重算
    /// 对齐，连续观看跨过窗口尾界时无感续解，不必等整批重新解压
    pub(super) fn compute_batch(
        &self,
        arch: &ArchiveState,
        entry_idx: usize,
        dir: &Path,
    ) -> (Vec<usize>, u64) {
        // 判缓存：尺寸一致视为已落地
        let on_disk = |i: usize| -> bool {
            let e = &arch.entries[i];
            let d = cache::dest_path(dir, arch.id, i, &e.name);
            std::fs::metadata(&d)
                .map(|m| m.len() == e.size)
                .unwrap_or(false)
        };
        let cap = self
            .cfg
            .extract
            .prefetch_downstream_mb
            .saturating_mul(1024 * 1024);
        let mut batch: Vec<usize> = Vec::new();
        let mut batch_bytes: u64 = 0;
        // 目标自身：未落地才入批（已缓存时窗口纯做预取，不占额度）
        if !on_disk(entry_idx) {
            batch.push(entry_idx);
            batch_bytes = arch.entries[entry_idx].size;
        }
        // 目标不在当前 view（被过滤）：单独成批，不扫后续
        let start = arch
            .view
            .iter()
            .position(|&i| i == entry_idx)
            .unwrap_or(usize::MAX);
        if start != usize::MAX && cap > 0 {
            for &i in arch.view[start + 1..].iter() {
                if batch_bytes >= cap {
                    break;
                }
                let e = &arch.entries[i];
                if e.is_dir || on_disk(i) {
                    continue;
                }
                batch_bytes += e.size;
                batch.push(i);
            }
        }

        // 上游预取：免费区 = 定位目标时必然被顺序解掉丢弃的上游条目
        // （locate_start 起点由格式决定——7z 目标所在 folder 起点、不跨 folder；
        // RAR 整个前缀），顺带写盘零解码增量。预算 prefetch_upstream_mb
        // 独立封顶，实取可能小于预算（folder 前缀不足时全取，不越界补齐）。
        // 注意与下游预取去重：sort_files 开启时 view 序 ≠ 物理序，
        // 下游批已收入的条目可能落在免费区内，重复入批会导致 file_done
        // 下标错位（7z/RAR 的 remaining 表按名去重，但 batch 下标对不齐）
        let upstream_cap = self
            .cfg
            .extract
            .prefetch_upstream_mb
            .saturating_mul(1024 * 1024);
        if upstream_cap > 0 {
            if let Some(s) = arch.vfs.locate_start(entry_idx) {
                let view_set: std::collections::HashSet<usize> =
                    arch.view.iter().copied().collect();
                let mut upstream_bytes: u64 = 0;
                for i in s..entry_idx {
                    if upstream_bytes >= upstream_cap {
                        break;
                    }
                    let e = &arch.entries[i];
                    // 仅预取当前列表可见的条目（目录/搜索过滤外的文件用户点不到）
                    if e.is_dir || !view_set.contains(&i) || batch.contains(&i) {
                        continue;
                    }
                    if on_disk(i) {
                        continue;
                    }
                    upstream_bytes += e.size;
                    batch_bytes += e.size;
                    batch.push(i);
                }
            }
        }
        (batch, batch_bytes)
    }

    /// 磁盘余量预检（整批）：缓存目录所在盘放不下（留 64MB 余量）则返回
    /// 错误消息。计入 LRU 可换出量——写入本批后超出缓存上限的部分必被
    /// evict_if_needed 驱逐（删除即释放磁盘），等效于凭空多出这部分余量；
    /// 不算它会在"盘满但缓存满额可换出"时误报不足。查不到余量（目录
    /// 不可达等）返回 None，让解压错误自己暴露。
    pub(super) fn disk_short(
        dir: &Path,
        batch_len: usize,
        batch_bytes: u64,
        quota_bytes: u64,
    ) -> Option<String> {
        let free = cache::free_bytes(dir)?;
        let evictable = Self::evictable_bytes(cache::dir_usage(dir), batch_bytes, quota_bytes);
        let free = free.saturating_add(evictable);
        let need = batch_bytes.saturating_add(64 * 1024 * 1024);
        (free < need).then(|| {
            format!(
                "缓存目录所在磁盘空间不足：剩余 {:.1}GB（含可 LRU 换出 {:.1}GB），本批 {} 个条目共需 {:.1}GB（缓存目录 [{}]，可清理缓存或修改 config.toml 的 cache.dir / extract.prefetch_downstream_mb）",
                free as f64 / (1024.0 * 1024.0 * 1024.0),
                evictable as f64 / (1024.0 * 1024.0 * 1024.0),
                batch_len,
                need as f64 / (1024.0 * 1024.0 * 1024.0),
                dir.display()
            )
        })
    }

    /// 本批写入后必被 LRU 驱逐的字节数 = min(占用, 占用+本批-上限)。
    /// 删除即释放磁盘，驱逐多少等效余量就多多少
    pub(super) fn evictable_bytes(usage: u64, batch_bytes: u64, quota_bytes: u64) -> u64 {
        usage.min(
            usage
                .saturating_add(batch_bytes)
                .saturating_sub(quota_bytes),
        )
    }

    /// 当前解码会话快照：(锚点 focus, 窗口摘要)。摘要格式
    /// "N 项，物理 a..=b"（targets 的物理下标范围）；无包/空窗口为
    /// None/"空"。供日志观测"后台此刻在解哪一段"
    pub(super) fn session_snapshot(&self) -> (Option<usize>, String) {
        let Some(arch) = self.archive.as_ref() else {
            return (None, "无会话".to_string());
        };
        let st = arch.session.lock().unwrap();
        let mut idxs: Vec<usize> = st.targets.values().map(|t| t.0).collect();
        idxs.sort_unstable();
        let win = match (idxs.first(), idxs.last()) {
            (Some(&lo), Some(&hi)) => format!("{} 项，物理 {lo}..={hi}", idxs.len()),
            _ => "空".to_string(),
        };
        (st.focus, win)
    }

    /// 启动/重定向后台解压任务。缓存命中（同尺寸文件已在）直接播放：
    /// 点击项在抢道锚点（focus，上次重定向目标）上游——即回看已看
    /// 内容——走 keep 分支完全不碰会话，后台继续解原计划（锚点只向
    /// 下游推进）；否则窗口照常重算——会话窗口以 focus 为中心（目标 +
    /// 上/下游预取），每次向下游推进都重新对齐滑动：批内切换/下游缓存
    /// 命中让窗口跟随用户推进，批外跳转则整体重算，共用同一条重定向
    /// 路径（solid 包定位目标需顺序解压上游前置数据，一次解一批摊薄
    /// 固定成本；上游条目定位时本就要被解掉丢弃，顺带写盘即"上游
    /// 预取"——随机跳看后往回翻直接命中缓存）
    pub(super) fn start_extract(&mut self, entry_idx: usize) {
        // 去重：该条目的解压线程已在跑（双击/重复点击会瞬时触发两次 play），
        // 不再起第二个线程——两个 unrar 抢写同一个临时文件会报 "Could not create file"。
        if self
            .extract
            .as_ref()
            .is_some_and(|j| j.entry_idx == entry_idx)
        {
            return;
        }
        // 只读规划段：借 archive 组批并产出 owned 数据。后续预检/重定向/
        // spawn 都要可变借用 self（方法调用借整个 self），须先结束 arch 借用
        let (cached, was_pending, session, vfs, dir, dest, name, total, targets, batch_bytes) = {
            let Some(arch) = &self.archive else { return };
            let entry = &arch.entries[entry_idx];
            let dir = cache::cache_dir(&self.cfg);
            let dest = cache::dest_path(&dir, arch.id, entry_idx, &entry.name);
            // 缓存命中：尺寸一致直接播（窗口照常重算，见函数 doc）
            let cached = std::fs::metadata(&dest)
                .map(|m| m.len() == entry.size)
                .unwrap_or(false);
            // 批内切换：新目标在当前批未完成清单里（覆盖层宽限期不清零的依据）
            let was_pending = self
                .extract
                .as_ref()
                .is_some_and(|j| j.pending.contains_key(&entry_idx));
            // 非 solid 包（AccessMode::SingleFile）：非 solid RAR 逐文件独立
            // 压缩（unrar 定位 = 廉价头跳转）；非 solid 7z 每文件独立 folder
            // （BlockDecoder 按 pack 偏移直达）。只解点击项 = 真随机访问，
            // 不组窗不预取（solid 包照旧 compute_batch 滑窗）
            let single = arch.vfs.access_mode() == AccessMode::SingleFile;
            let (targets, batch_bytes) = if single {
                (
                    vec![(
                        entry.name.clone(),
                        entry_idx,
                        cache::dest_path(&dir, arch.id, entry_idx, &entry.name),
                    )],
                    entry.size,
                )
            } else {
                let (batch, batch_bytes) = self.compute_batch(arch, entry_idx, &dir);
                let targets: Vec<(String, usize, PathBuf)> = batch
                    .iter()
                    .map(|&i| {
                        let e = &arch.entries[i];
                        (
                            e.name.clone(),
                            i,
                            cache::dest_path(&dir, arch.id, i, &e.name),
                        )
                    })
                    .collect();
                (targets, batch_bytes)
            };
            (
                cached,
                was_pending,
                arch.session.clone(),
                arch.vfs.clone(),
                dir,
                dest,
                entry.name.clone(),
                entry.size,
                targets,
                batch_bytes,
            )
        };

        // 回看 keep 判定：缓存命中 + 会话存活 + 点击项在抢道锚点
        // （focus，上次重定向写入、view 位在其上游）→ 完全不碰会话——
        // 锚点只向下游推进不回退，后台继续按原计划解压锚点窗口（跳点
        // 80 等解的同时回翻 1-30 缓存即此场景）。锚点与播放解耦：列表
        // 高亮/滚动跟随 self.playing，focus 仅由解码线程消费（见
        // SessionState::focus）
        let (sess_alive, sess_focus) = {
            let st = session.lock().unwrap();
            (st.alive, st.focus)
        };
        let (click_pos, focus_pos) = {
            let pos_of = |i: usize| {
                self.archive
                    .as_ref()
                    .and_then(|a| a.view.iter().position(|&v| v == i))
            };
            (pos_of(entry_idx), sess_focus.and_then(pos_of))
        };
        let upstream = matches!((click_pos, focus_pos), (Some(p), Some(f)) if p < f);
        if cached && sess_alive && upstream {
            // 仅切 UI 侧任务字段：Done 兜底补播以 j.entry_idx 为准，不切
            // 会在旧目标落地时把画面从当前条目拽走。pending/batch_len/gen
            // 都不动——顶栏"剩余 N 项"继续如实反映后台解压量；j.gen 与
            // next_gen 不变，后台整批解完的 Done 兜底补播因 playing 已是
            // 本条目而天然不触发
            let pend = self.extract.as_ref().map(|j| j.pending.len()).unwrap_or(0);
            let (_, win) = self.session_snapshot();
            if let Some(j) = self.extract.as_mut() {
                j.entry_idx = entry_idx;
                j.dest = dest.clone();
                j.entry_name = name.clone();
                j.total = total;
                j.done = 0;
                j.stage = String::new();
            }
            tracing::debug!(
                "缓存命中（回看，会话保持）: {name}｜点击 view 位 {}，锚点 focus={}（view 位 {}），切换前 playing={:?}，后台待落地 {pend} 项，窗口 {win}",
                click_pos.unwrap_or(usize::MAX),
                sess_focus.unwrap_or(usize::MAX),
                focus_pos.unwrap_or(usize::MAX),
                self.playing
            );
            self.load_extracted(&dest, entry_idx);
            return;
        }

        // 整窗已落地：无事可解，不重定向会话（空 targets 会让解码线程判空
        // 退出）。也不动 focus 锚点——本就无事可解，锚点保持让后台按原
        // 计划收尾；仅播放 + 切 j 字段——Done 兜底补播以 j.entry_idx 为
        // 准，不切会在旧目标落地时把画面从当前条目拽走
        if targets.is_empty() {
            debug_assert!(cached);
            let (_, win) = self.session_snapshot();
            if let Some(j) = self.extract.as_mut() {
                j.entry_idx = entry_idx;
                j.dest = dest.clone();
                j.entry_name = name.clone();
                j.total = total;
                j.done = 0;
                j.stage = String::new();
            }
            tracing::debug!(
                "缓存命中（整窗已落地）: {name}｜锚点 focus={:?}，playing={:?}，当前窗口 {win}",
                sess_focus,
                self.playing
            );
            self.load_extracted(&dest, entry_idx);
            return;
        }

        // 磁盘余量预检（整批）：批外目标放不下直接报错，不发起到注定失败的
        // 解压；缓存命中降级——播放不受影响，沿用旧窗口不滑动
        if let Some(msg) = Self::disk_short(
            &dir,
            targets.len(),
            batch_bytes,
            self.cfg.cache.max_size_mb * 1024 * 1024,
        ) {
            if cached {
                tracing::warn!(
                    "窗口滑动跳过（磁盘不足，播放不受影响）：{msg}｜锚点 focus={:?}，playing={:?}",
                    sess_focus,
                    self.playing
                );
                self.load_extracted(&dest, entry_idx);
                return;
            }
            tracing::error!("{msg}");
            self.error = Some(msg);
            return;
        }

        cache::evict_if_needed(&dir, self.cfg.cache.max_size_mb * 1024 * 1024, batch_bytes);

        // 会话重定向（三分支共用）：清空旧目标写入新窗口 + focus。批内切换/
        // 缓存命中时新窗口 ≈ 旧窗口平移（已落地头部出窗、尾部补新），批外
        // 跳转则整体重算，对解码协议是同一件事。"写目标 + 查 alive + 需要
        // 时置 true"同一次持锁完成，与解码线程的"判空退出"互斥，杜绝窗口
        // 期目标丢失（协议见 vfs/mod.rs SessionState）
        let batch_len = targets.len();
        let win_summary = {
            let mut idxs: Vec<usize> = targets.iter().map(|(_, i, _)| *i).collect();
            idxs.sort_unstable();
            match (idxs.first(), idxs.last()) {
                (Some(&lo), Some(&hi)) => format!("{batch_len} 项，物理 {lo}..={hi}"),
                _ => format!("{batch_len} 项"),
            }
        };
        let alive = {
            let mut st = session.lock().unwrap();
            st.targets.clear();
            for (n, i, d) in &targets {
                st.targets.insert(n.clone(), (*i, d.clone()));
            }
            st.focus = Some(entry_idx);
            // 「回到锚点」目标同步推进：仅目标未缓存（真正需要解压）时写。
            // 缓存命中秒开只是回看/预取重定向，不算解压推进——否则会话
            // 退出后翻看已缓存内容会把锚点污染掉，回不到真正的解压位置
            if !cached {
                self.last_focus = Some(entry_idx);
                // 新解压开始：旧的"解压完成待消费"提醒作废
                self.extract_ready = false;
            }
            // 重定向代数 +1：解码线程每轮规划快照 gen，零落地时据此区分
            // "UI 换了目标"与"目标不在解码流中"（后者才报错，见 vfs/mod.rs）
            st.gen += 1;
            let alive = st.alive;
            if !alive {
                st.alive = true;
            }
            alive
        };
        let pending: std::collections::HashMap<usize, PathBuf> =
            targets.iter().map(|(_, i, d)| (*i, d.clone())).collect();
        self.next_gen += 1;
        let gen = self.next_gen;

        if alive {
            // 解码线程仍在：重定向目标集即可，解码流续跑（solid 包重启 =
            // 重新顺序解前置数据，秒到分钟级浪费）。消息通道/取消标志复用
            // 现任务；pending 重建——旧批未落项已不在 targets 永不落地，
            // 残留会让"批内切换"误判（pending 命中却等不到 FileDone）
            if let Some(j) = self.extract.as_mut() {
                j.gen = gen;
                j.entry_idx = entry_idx;
                j.dest = dest.clone();
                j.entry_name = name.clone();
                j.total = total;
                j.done = 0;
                j.stage = String::new();
                j.pending = pending;
                j.batch_len = batch_len;
                // started 仅批外新目标重置：批内切换/缓存命中沿用（覆盖层
                // 宽限期不清零，立即显示进度是对的）
                if !was_pending && !cached {
                    j.started = std::time::Instant::now();
                }
                if cached {
                    tracing::debug!(
                        "缓存命中: {name}｜锚点 focus {}→{entry_idx}，切换前 playing={:?}，窗口 {win_summary}",
                        sess_focus.unwrap_or(usize::MAX),
                        self.playing
                    );
                    self.load_extracted(&dest, entry_idx);
                } else if was_pending {
                    tracing::debug!(
                        "批内切换目标: {name}（窗口滑动至 {win_summary}）｜锚点 focus {}→{entry_idx}，切换前 playing={:?}",
                        sess_focus.unwrap_or(usize::MAX),
                        self.playing
                    );
                } else {
                    tracing::debug!(
                        "会话重定向: {name}（新批 {win_summary}，解码流续跑）｜锚点 focus {}→{entry_idx}，切换前 playing={:?}",
                        sess_focus.unwrap_or(usize::MAX),
                        self.playing
                    );
                }
                return;
            }
            // 走不到（alive=true ⟺ 线程在跑 ⟺ 任务在）：防御性落到重启路径
        }

        // 解码线程已退出（空闲/出错/从未启动）：新线程 + 新通道消费会话。
        // 缓存命中也照常起会话——窗口围绕已播目标继续预取，连续观看缓存
        // 内容时跨出窗口同样无感滑动
        if cached {
            tracing::debug!(
                "缓存命中（预取会话启动）: {name}｜锚点 focus {}→{entry_idx}，切换前 playing={:?}，新窗 {win_summary}",
                sess_focus.unwrap_or(usize::MAX),
                self.playing
            );
            self.load_extracted(&dest, entry_idx);
        }
        let (tx, rx) = mpsc::channel::<ExtMsg>();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = cancel.clone();
        let session_for_thread = session;
        std::thread::Builder::new()
            .name("extract".into())
            .spawn(move || {
                let r = vfs.extract_session(
                    &session_for_thread,
                    &cancel_for_thread,
                    &|done, stage| {
                        let _ = tx.send(ExtMsg::Progress(done, stage.to_string()));
                    },
                    &|idx, _path| {
                        let _ = tx.send(ExtMsg::FileDone(idx));
                    },
                );
                match r {
                    Ok(()) => {
                        let _ = tx.send(ExtMsg::Done);
                    }
                    Err(_) if cancel_for_thread.load(std::sync::atomic::Ordering::Relaxed) => {
                        // 切换压缩包（cancel 置位）：会话随旧包丢弃，静默退出
                    }
                    Err(e) => {
                        let is_pwd = matches!(e, VfsError::Password);
                        let _ = tx.send(ExtMsg::Failed(e.to_string(), is_pwd));
                    }
                }
            })
            .ok();

        if !cached {
            tracing::debug!(
                "后台解压: {name} → {}（顺带预取 {} 项）｜锚点 focus {}→{entry_idx}，切换前 playing={:?}，窗口 {win_summary}",
                dest.display(),
                batch_len - 1,
                sess_focus.unwrap_or(usize::MAX),
                self.playing
            );
        }
        self.extract = Some(ExtractJob {
            gen,
            entry_idx,
            dest,
            entry_name: name,
            total,
            done: 0,
            stage: String::new(),
            pending,
            batch_len,
            started: std::time::Instant::now(),
            cancel,
            rx,
        });
    }

    /// 每帧轮询解压任务状态：循环排空通道内全部消息（Progress 是覆盖语义，
    /// 排空中只留最新一条；FileDone/Done/Failed 逐条即时处理）
    pub(super) fn poll_extract(&mut self) {
        loop {
            let Some(job) = &mut self.extract else { return };
            let msg = match job.rx.try_recv() {
                Ok(m) => m,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    ExtMsg::Failed(format!("解压线程中断（条目 [{}]）", job.entry_name), false)
                }
            };
            let (gen, entry_idx, dest, entry_name) = (
                job.gen,
                job.entry_idx,
                job.dest.clone(),
                job.entry_name.clone(),
            );
            match msg {
                ExtMsg::Progress(done, stage) => {
                    if let Some(j) = &mut self.extract {
                        // 写文件阶段（stage 空）的字节数按目标尺寸封顶：批内
                        // 前置预取文件先于目标落地，其写入进度若直接进目标
                        // 进度条会出现"300MB / 60MB"的怪相（跳过阶段 stage
                        // 非空、按字节文本展示，不受影响）
                        j.done = if stage.is_empty() {
                            done.min(j.total)
                        } else {
                            done
                        };
                        j.stage = stage;
                    }
                    // 继续排空而非 return：Progress 若每帧只消费一条（10fps
                    // ≈ 10条/秒），高速率落地时通道积压分钟级——线程退出后
                    // 浮层仍转圈（用户误判会话活着，实际 alive 已 false）、
                    // Done 延迟不达，且下一次点击 spawn 新会话换 rx 时积压
                    // 连同 Done 被整个丢弃（"会话解净空闲退出"日志消失）
                }
                ExtMsg::FileDone(i) => {
                    // 批内条目落地：是当前期望目标则立即播放（预取的留在缓存里
                    // 供后续翻页命中），否则只记账
                    let mut play: Option<(PathBuf, usize)> = None;
                    if let Some(j) = &mut self.extract {
                        if let Some(d) = j.pending.remove(&i) {
                            // 锚点落地但用户在看别处（回看中，keep 改写了期望
                            // 目标，下面的自动播放被吞）：置"解压完成待消费"
                            // 提醒——顶栏一键回锚点，避免解压成果闲置
                            if Some(i) == self.last_focus && i != j.entry_idx {
                                self.extract_ready = true;
                            }
                            // 缓存命中路径已直接播放该条目时不重播
                            // （重播会让视频闪断、图片重置动画）
                            if i == j.entry_idx && self.playing != Some(i) {
                                play = Some((d, i));
                            }
                        }
                    }
                    if let Some((d, i)) = play {
                        self.load_extracted(&d, i);
                    }
                }
                ExtMsg::Done => {
                    // 回看提醒挂起（锚点落地被 keep 吞、待用户消费）时兜底
                    // 补播必须让位：此刻强切 = 把用户从回看内容拽回锚点，
                    // 还会经 load_extracted 清掉提醒——表现为"预取一完成
                    // 画面被拽走、顶栏按钮消失"
                    let reviewing = self.extract_ready;
                    self.extract = None;
                    // 会话空闲退出（targets 解空，线程已置 alive=false）。
                    // 显式留痕：此后 keep 分支（要求会话存活）失效，下一次
                    // 缓存命中点击会走 ⑤d 重新拉起预取会话并重写锚点——
                    // 没有这条日志，该转折在日志流里完全不可见
                    let (focus, win) = self.session_snapshot();
                    tracing::debug!(
                        "会话解净空闲退出（窗口清空）: 最后目标 [{entry_name}]｜锚点 focus={focus:?}，当前窗口 {win}，playing={:?}",
                        self.playing
                    );
                    // 兜底：目标理论上已由 FileDone 播放（gen 匹配 = 期望
                    // 未被后续重定向取代且未被批内切走）；文件在而没播过才补播
                    if !reviewing && gen == self.next_gen && self.playing != Some(entry_idx) {
                        if std::fs::metadata(&dest).is_ok() {
                            self.load_extracted(&dest, entry_idx);
                        }
                    }
                }
                ExtMsg::Failed(e, is_pwd) => {
                    self.extract = None;
                    if is_pwd {
                        // 内容加密条目（7z/RAR）：弹密码框，提交后重建 VFS 重试
                        self.pw_prompt = Some(PwPrompt {
                            kind: PwKind::Play(entry_idx),
                            input: String::new(),
                            wrong: true,
                        });
                    } else {
                        // 日志带完整上下文（压缩包/条目/目标路径）；
                        // 用户可见错误直接展示 VFS 层消息（其中已含条目与路径细节）
                        let arch = self
                            .archive
                            .as_ref()
                            .map(|a| a.name.as_str())
                            .unwrap_or("<未知压缩包>");
                        tracing::error!(
                            "解压失败: 压缩包 [{arch}] 条目 [{entry_name}] → [{}]: {e}",
                            dest.display()
                        );
                        self.error = Some(format!("解压失败: {e}"));
                    }
                }
            }
        }
    }

    /// 密码提交后：带密码重开压缩包（重建 VFS）并重试播放指定 entry。
    /// 重开同样走后台线程（zip 密码校验在 open_job 线程内做，错密码不替换状态、直接重弹）
    pub(super) fn replay_with_password(&mut self, entry_idx: usize, pw: String) {
        let Some(arch) = self.archive.as_ref() else {
            return;
        };
        let path = arch.path.clone();
        let entry = arch.entries[entry_idx].clone();
        self.spawn_open(path, Some(pw), Some((entry_idx, entry)));
    }

    /// 播放已解压到本地的文件。视频走 mpv load_path；图片（纹理路由）停掉
    /// mpv 画面后从落盘文件后台解码——7z/RAR 图片的公共出口（解压落地
    /// FileDone、缓存命中、锚点回放都汇到这里）
    pub(super) fn load_extracted(&mut self, dest: &Path, entry_idx: usize) {
        // 播放到锚点 = "解压完成待消费"提醒消费完毕（start_extract 缓存
        // 命中不经 play()，提醒按钮/B 键/悬浮按钮/列表点击最终都汇到这里）
        if self.last_focus == Some(entry_idx) {
            self.extract_ready = false;
        }
        self.playing = Some(entry_idx);
        self.dragging = false;
        // 先取 owned 元组结束不可变借用；无 archive（防御）退化为 mpv 直载
        let routed = self.archive.as_ref().map(|a| {
            (
                (a.id, entry_idx),
                crate::imgview::wants_texture(&self.cfg, &a.entries[entry_idx].name),
                a.entries[entry_idx].name.clone(),
            )
        });
        // 曾解码失败回退 mpv 的条目不再走纹理解码，直接落到下方 mpv 装载
        let routed = routed.filter(|(key, _, _)| !self.img_fallback.contains(key));
        if let Some((key, true, name)) = routed {
            // 图片：停 mpv 画面（可能有上一张视频/动图），落盘文件后台解码
            if let Some(p) = &self.player {
                p.stop();
            }
            let max_dim = self.cfg.ui.image_max_dim;
            let src = ImageSource::File(dest.to_path_buf());
            // 记录解码源：放大到 1:1 附近时按需再解原图（image_view 触发）
            self.img_source = Some((key, src.clone(), name.clone()));
            if !self.img.has(key) && !self.img.is_pending(key) {
                self.img.request(key, src, max_dim, &name);
            }
            self.img_view = ImgView::default();
            tracing::info!("图片装载: {name}");
            self.prefetch_images();
            return;
        }
        // 落盘视频走 mpv：同流式路径清快照防隔图闪现
        self.hold_img = None;
        self.img_source = None;
        if let Some(p) = &self.player {
            p.load_path(dest);
        }
    }
}
