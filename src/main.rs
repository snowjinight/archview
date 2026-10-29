//! 压缩包媒体预览播放器 — Phase 1 MVP
//! ZIP 流式播放 + egui 列表/搜索 + 进度条 seek。

mod cache;
mod config;
mod imgview;
mod player;
mod ui;
mod vartidy;
mod vfs;

fn main() -> eframe::Result<()> {
    // CLI 子命令分发：`archview tidy ...` = var 资源包增量整理（ARCHITECTURE.md 第 12 章），
    // 无参数 = GUI 播放器（行为不变）。
    // GUI 模式下命令行中的压缩包/目录作为初始打开目标
    // （SendTo「Archview player」快捷方式 / 拖到 exe 上 / 文件关联）
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("tidy") {
        let code = vartidy::cli_main(&args[1..]);
        std::process::exit(code);
    }
    let initial = initial_archive(&args);

    // config 需在 tracing 初始化前加载（日志级别来自配置）——此时日志管线
    // 尚未建立，失败信息走 eprintln 兜底
    let cfg = match config::Config::load_or_create() {
        Ok(c) => c,
        // 解析失败即 panic（用户要求）：但先打明确原因——
        // TOML 错误带行列号，常见于字符串没加双引号、反斜杠没双写等转义问题
        Err(e) => {
            eprintln!("config.toml 加载失败: {e}");
            panic!("配置加载失败: {e}");
        }
    };
    // 日志级别优先级：RUST_LOG 环境变量 > config.toml log_level > 默认 info。
    // 均走 EnvFilter 解析：级别名单（info/debug/…）或按模块指令都接受；
    // 非法值回退 info
    let log_filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| Some(cfg.log_level.clone()))
        .and_then(|s| tracing_subscriber::EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(log_filter).init();
    if let Err(e) = player::mpv_ffi::init(&cfg.dll_path()) {
        // mpv 不可用时仍可浏览列表，播放功能在 UI 内提示错误
        tracing::error!("{e}");
    }

    // 编译期内嵌 PNG 图标：标题栏 + 任务栏 + Alt+Tab 共用（eframe 在 Windows 上
    // 自动按系统尺寸缩放为 ICON_BIG/ICON_SMALL），只支持 PNG、需透明通道
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("assets/icon.png 解码失败");

    let opts = eframe::NativeOptions {
        // glow 渲染器：mpv render API 需要 GL 上下文（wgpu 默认下无 glow painter）
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("Archive Media Player")
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "archview",
        opts,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cfg, cc, initial)))),
    )
}

/// GUI 启动参数解析：压缩包文件直接打开；目录即媒体集合（递归收集图片/视频，
/// 跳过压缩包——与拖放/打开目录对话框同一条规则，ARCHITECTURE.md 4.2）。
/// 混合多选（目录+压缩包同时"发送到"）按参数顺序取第一个有效项，其余忽略。
/// 非路径/非压缩包参数忽略——保持无参数启动行为完全不变
fn initial_archive(args: &[String]) -> Option<std::path::PathBuf> {
    let is_archive = |p: &std::path::Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| matches!(e.as_str(), "zip" | "7z" | "rar"))
    };
    for a in args {
        let p = std::path::Path::new(a);
        if (p.is_file() && is_archive(p)) || p.is_dir() {
            return Some(p.to_path_buf());
        }
    }
    None
}
