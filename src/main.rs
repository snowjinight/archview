//! 压缩包媒体预览播放器 — Phase 1 MVP
//! ZIP 流式播放 + egui 列表/搜索 + 进度条 seek。

mod cache;
mod config;
mod player;
mod ui;
mod vfs;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::load_or_create().expect("配置加载失败");
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
