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
    // 无参数 = GUI 播放器（行为不变）
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("tidy") {
        let code = vartidy::cli_main(&args[1..]);
        std::process::exit(code);
    }

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
        Box::new(move |cc| Ok(Box::new(ui::App::new(cfg, cc)))),
    )
}
