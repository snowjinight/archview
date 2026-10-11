//! 构建时把 libmpv-2.dll 复制到 target 目录（运行时 exe 旁加载）。
//! 源目录默认为包根 deps/mpv-dev，可用环境变量 ZIP_VIDEO_MPV_DIR 覆盖，不硬编码绝对路径。

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=ZIP_VIDEO_MPV_DIR");
    let src_dir = std::env::var("ZIP_VIDEO_MPV_DIR").unwrap_or_else(|_| "deps/mpv-dev".into());
    let src = Path::new(&src_dir).join("libmpv-2.dll");
    println!("cargo:rerun-if-changed={}", src.display());
    let Some(out) = std::env::var_os("OUT_DIR") else {
        return;
    };
    // OUT_DIR = target/<profile>/build/<hash>/out → target/<profile>
    let Some(target_dir) = Path::new(&out).ancestors().nth(3) else {
        return;
    };
    if src.exists() {
        let dst = target_dir.join("libmpv-2.dll");
        match std::fs::copy(&src, &dst) {
            Ok(n) => println!(
                "cargo:warning=已复制 libmpv-2.dll ({n} 字节) 到 {}",
                dst.display()
            ),
            Err(e) => println!("cargo:warning=复制 libmpv-2.dll 失败: {e}"),
        }
    } else {
        println!("cargo:warning=未找到 {src_dir}\\libmpv-2.dll，请确认目录");
    }
}
