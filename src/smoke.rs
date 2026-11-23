//! 冒烟测试夹具定位（#[cfg(test)]）。
//!
//! 两段式流程（doc/架构落地开发/06_测试运行方式与覆盖矩阵.md）：
//! 1. 生成阶段 `cargo run --bin smoke_setup`——依赖检查 + 清理重建 smoke/ +
//!    按测试族生成夹具（src/bin/smoke_setup.rs，不依赖本 crate 代码）；
//! 2. 测试阶段 `cargo test`——测试经本模块 require 系列定位夹具，
//!    **缺失即 panic 带指引，禁止静默跳过**（setup 已前置保证存在，
//!    测试期缺失说明环境被动过，必须显式报错）。
//!
//! smoke/ 目录按测试族分子目录（player_e2e/encode_e2e/rar/img/diag/vartidy），
//! libmpv-2.dll 每族独立副本，避免并发测试抢占公共目录/公共 DLL。

use std::path::PathBuf;

/// smoke/ 根目录（CARGO_MANIFEST_DIR/smoke，与 setup 生成器同源，无硬编码盘符）
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("smoke")
}

/// 定位夹具文件/目录，缺失即 panic（统一指引文案）
pub fn require(rel: &str) -> PathBuf {
    let p = root().join(rel);
    if !p.exists() {
        panic!(
            "冒烟夹具缺失: {}\n  先运行: cargo run --bin smoke_setup（或 run_tests.bat）再执行测试",
            p.display()
        );
    }
    p
}

/// 定位测试族的 libmpv-2.dll 所在目录（每族独立副本，防并发抢占公共 DLL）
pub fn require_dll_dir(family: &str) -> PathBuf {
    let dir = require(family);
    let dll = dir.join("libmpv-2.dll");
    assert!(
        dll.exists(),
        "冒烟夹具缺失: {}\n  先运行: cargo run --bin smoke_setup（或 run_tests.bat）再执行测试",
        dll.display()
    );
    dir
}

/// 读 smoke/env.toml [tools] 登记的可选工具路径（smoke_setup 写入）。
/// 缺键 / 文件不存在 → None（调用方按可选项语义处理，如 ffprobe 软降级）。
pub fn tool(name: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root().join("env.toml")).ok()?;
    let section = text.split("[tools]").nth(1)?;
    for line in section.lines() {
        // 形如 `  ffprobe = 'C:\tools\ffprobe.exe'`（路径用单引号 literal string）
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == name {
            let v = v.trim().trim_matches('\'').trim_matches('"');
            let p = PathBuf::from(v);
            return p.exists().then_some(p);
        }
    }
    None
}
