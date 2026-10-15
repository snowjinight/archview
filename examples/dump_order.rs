//! 诊断工具：打印压缩包头信息里的原始条目顺序（即播放器 sort_files=false
//! 时文件列表的数据源；也是固态包预取批的物理相邻顺序）。
//!
//! 背景：7-Zip/WinRAR 等 GUI 软件的列表是软件自行排序后的显示效果
//! （文件夹优先 + 名称排序），不代表包的真实内部顺序。
//!
//! 用法：cargo run --example dump_order -- <压缩包路径> [显示条数，默认 50]
//! 支持 .7z / .zip / .rar（加密包暂不支持，需明文包）

use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("用法: cargo run --example dump_order -- <7z/zip/rar包路径> [显示条数]");
        return;
    };
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50);

    let lower = path.to_lowercase();
    if lower.ends_with(".7z") {
        dump_7z(path, n);
    } else if lower.ends_with(".zip") {
        dump_zip(path, n);
    } else if lower.ends_with(".rar") {
        dump_rar(path, n);
    } else {
        eprintln!("不支持的格式: {path}（支持 .7z / .zip / .rar）");
    }
}

fn dump_7z(path: &str, n: usize) {
    let reader =
        match sevenz_rust::SevenZReader::open(Path::new(path), sevenz_rust::Password::empty()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("打开失败（加密头需密码，暂不支持）: {e}");
                return;
            }
        };
    println!("== 7z 头信息原始顺序（前 {n} 条）==");
    for (i, f) in reader.archive().files.iter().take(n).enumerate() {
        let tag = if f.is_directory { "[DIR] " } else { "      " };
        println!("{:4} {}{}", i, tag, f.name);
    }
}

fn dump_zip(path: &str, n: usize) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("打开失败: {e}");
            return;
        }
    };
    let mut zip = match zip::ZipArchive::new(file) {
        Ok(z) => z,
        Err(e) => {
            eprintln!("读取失败（加密包暂不支持）: {e}");
            return;
        }
    };
    println!("== ZIP 中央目录原始顺序（前 {n} 条，[DIR] = 目录）==");
    for i in 0..zip.len().min(n) {
        let f = zip.by_index_raw(i).unwrap();
        let tag = if f.is_dir() { "[DIR] " } else { "      " };
        println!("{:4} {}{}", i, tag, f.name());
    }
}

fn dump_rar(path: &str, n: usize) {
    let arch = unrar::Archive::new(Path::new(path));
    let Ok(mut open) = arch.open_for_listing() else {
        eprintln!("打开失败（加密头包暂不支持）");
        return;
    };
    println!("== RAR 头信息原始顺序（前 {n} 条，[DIR] = 目录）==");
    let mut i = 0;
    while i < n {
        match open.read_header() {
            Ok(Some(head)) => {
                let e = head.entry();
                let tag = if e.is_directory() { "[DIR] " } else { "      " };
                println!("{:4} {}{}", i, tag, e.filename.to_string_lossy());
                i += 1;
                let Ok(next) = head.skip() else {
                    eprintln!("跳过头失败");
                    return;
                };
                open = next;
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("读头失败（加密头包暂不支持）: {e}");
                return;
            }
        }
    }
}
