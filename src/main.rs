//! 压缩包媒体预览播放器 — Phase 1 MVP
//! ZIP 流式播放 + egui 列表/搜索 + 进度条 seek。

mod cache;
mod config;
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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = match config::Config::load_or_create() {
        Ok(c) => c,
        // 解析失败即 panic（用户要求）：但先打明确原因——
        // TOML 错误带行列号，常见于字符串没加双引号、反斜杠没双写等转义问题
        Err(e) => {
            tracing::error!("config.toml 加载失败: {e}");
            eprintln!("config.toml 加载失败: {e}");
            panic!("配置加载失败: {e}");
        }
    };
    if let Err(e) = player::mpv_ffi::init(&cfg.dll_path()) {
        // mpv 不可用时仍可浏览列表，播放功能在 UI 内提示错误
        tracing::error!("{e}");
    }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("Archive Media Player"),
        ..Default::default()
    };
    eframe::run_native(
        "archview",
        opts,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cfg, cc, initial)))),
    )
}

/// GUI 启动参数解析：第一个存在的 zip/7z/rar 文件；目录则取其中首个压缩包
/// （与 GUI「目录内连续看包」衔接，其余包按 N 键可达）。
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
        if p.is_file() && is_archive(p) {
            return Some(p.to_path_buf());
        }
        if p.is_dir() {
            // read_dir 顺序不保证，按名排序取首个，行为可预期
            let mut found: Vec<_> = std::fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|path| path.is_file() && is_archive(path))
                .collect();
            found.sort();
            if let Some(first) = found.first() {
                return Some(first.clone());
            }
        }
    }
    None
}
