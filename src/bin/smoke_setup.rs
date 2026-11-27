//! 冒烟测试夹具生成器（两段式流程第 1 段，产物落 smoke/，已 gitignore）。
//! 用法：cargo run --bin smoke_setup（或直接 run_tests.bat）
//!
//! 流程：清空重建 smoke/ → 依赖检查（必需缺失即 exit(1) 汇总列出；
//! 可选缺失仅警告）→ 写 env.toml → 按测试族生成夹具。
//!
//! 结构类夹具参数与各测试的断言一致（如 rar payload 字节 / anim2.webp 的
//! 2 帧 20x14 100ms），改动前先核对对应测试断言：
//!   player_e2e/  libmpv-2.dll + clip.mp4 + test.zip（两 entry Deflated）
//!                —— src/player/mod.rs e2e 系列（make_test_zip 同参数）
//!   encode_e2e/  libmpv-2.dll —— src/encode/mod.rs e2e（dump 帧纯 Rust 构造）
//!   rar/         test.rar + plain.rar + solid.rar
//!                —— src/vfs/rar_vfs.rs list_and_extract / non_solid / solid
//!   img/         anim2.webp —— src/imgview.rs decode_动画webp逐帧
//!   diag/        641.webp + 1000.webp —— src/player/mod.rs diag_webp_load_latency
//!   vartidy/     pack1-6 + var_repo —— run_tests.bat 第 3 段 VarTidy CLI 冒烟
//!                （吸收自原 examples/make_smoke_fixtures.rs）
//!
//! 随机化边界：外观类内容（视频 hue / anim2 色值）每次生成随机——测试断言
//! 只锚定时长/尺寸/结构/通道优势区间，随机内容能锻炼不同码流；
//! 长度与字节内容锚定的夹具（rar 填充字节、vartidy 去重包对）保持固定。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 简易随机源（纳秒+pid 种子 xorshift64），仅用于外观类夹具随机化
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        Rng(nanos ^ ((std::process::id() as u64) << 32) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// [lo, hi) 均匀整数
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo)
    }
}

/// 随机视频色调滤镜参数（testsrc2 基础上整体偏色，每次生成画面不同）
fn hue_vf(rng: &mut Rng) -> String {
    format!("hue=h={:.1}", rng.range(0, 3600) as f64 / 10.0)
}

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("smoke");

    // ---- 1. 清空重建 smoke/（bat 在仓库根执行，无"删掉执行中脚本"问题）----
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap_or_else(|e| {
            panic!("无法清理旧夹具 {}（测试可能正在运行）: {e}", root.display())
        });
    }
    std::fs::create_dir_all(&root).unwrap();
    println!("[clean] 已清空重建 {}", root.display());

    // ---- 2. 依赖检查：必需缺失 → 汇总列出后 exit(1)；可选缺失 → 警告 ----
    let mut missing: Vec<&str> = Vec::new();

    let ffmpeg_opt = resolve_on_path("ffmpeg").or_else(|| probe_ok("ffmpeg", &["-version"]));
    if ffmpeg_opt.is_none() {
        missing.push("ffmpeg（生成视频/webp 夹具必需，装好后加入 PATH）");
    }

    let mpv_opt = find_mpv_dll();
    if mpv_opt.is_none() {
        missing.push(
            "libmpv-2.dll（mpv e2e 必需；下载 mpv dev 包解到 deps/mpv-dev/，\
             或设 ZIP_VIDEO_MPV_DIR 指向其所在目录）",
        );
    }

    if !missing.is_empty() {
        eprintln!("\n=== 依赖缺失，中止（共 {} 项）===", missing.len());
        for m in &missing {
            eprintln!("  - {m}");
        }
        std::process::exit(1);
    }
    let ffmpeg = ffmpeg_opt.unwrap();
    let mpv_dll = mpv_opt.unwrap();

    // 可选：ffprobe（encode e2e 精确帧数断言，缺则测试软降级）
    let ffprobe = resolve_on_path("ffprobe")
        .or_else(|| probe_ok("ffprobe", &["-version"]))
        .or_else(|| ffmpeg.parent().map(|d| d.join("ffprobe.exe")).filter(|p| p.exists()));
    if ffprobe.is_none() {
        eprintln!("[warn] ffprobe 未找到：encode e2e 将跳过精确帧数断言（软降级，不阻塞）");
    }
    // 可选：WinRAR rar.exe（rar 族夹具，缺则该族不生成、对应测试将 panic）
    let rar = find_rar_exe();
    if rar.is_none() {
        eprintln!(
            "[warn] WinRAR rar.exe 未找到（可设 ARCHVIEW_TEST_RAR）：\
             smoke/rar 夹具不生成，rar 相关测试将报「冒烟夹具缺失」"
        );
    }

    // ---- 3. env.toml（工具路径登记，src/smoke.rs::tool() 读取）----
    let mut env = String::new();
    env.push_str("# smoke/env.toml — smoke_setup 生成，src/smoke.rs::tool() 读取（勿手改）\n[tools]\n");
    env.push_str(&format!("ffmpeg  = '{}'\n", toml_lit(&ffmpeg)));
    if let Some(p) = &ffprobe {
        env.push_str(&format!("ffprobe = '{}'\n", toml_lit(p)));
    }
    if let Some(p) = &rar {
        env.push_str(&format!("rar     = '{}'\n", toml_lit(p)));
    }
    env.push_str("\n[paths]\n");
    env.push_str(&format!("mpv_dll_src = '{}'\n", toml_lit(&mpv_dll)));
    std::fs::write(root.join("env.toml"), env).unwrap();
    println!("[env] env.toml 已写入");

    // ---- 4. 按测试族生成夹具（族间目录/DLL 隔离，防并发抢占）----
    let mut rng = Rng::new();
    gen_player(&root, &ffmpeg, &mpv_dll, &mut rng);
    place_dll(&root.join("encode_e2e"), &mpv_dll);
    println!("[encode_e2e] libmpv-2.dll 副本就绪");

    if let Some(rar) = &rar {
        gen_rar(&root, rar);
    } else {
        println!("[rar] 跳过（无 rar.exe）");
    }

    gen_img(&root, &ffmpeg, &mut rng);
    gen_diag(&root, &ffmpeg, &mut rng);
    gen_vartidy(&root);

    println!("\n=== 冒烟夹具生成完毕: {} ===", root.display());
}

// ---- 依赖探测 ----

/// `where` 解析 PATH 上的工具绝对路径（Windows）
fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let out = Command::new("where").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_owned();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// 直接跑子命令验证可用（where 之外的兜底，如 PATH 未登记的别名）
fn probe_ok(name: &str, args: &[&str]) -> Option<PathBuf> {
    Command::new(name)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| PathBuf::from(name))
}

/// libmpv-2.dll 定位：ZIP_VIDEO_MPV_DIR → deps/mpv-dev → target/{debug,release}
/// → 当前 exe 旁（build.rs 同源逻辑）
fn find_mpv_dll() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cands: Vec<PathBuf> = vec![];
    if let Ok(d) = std::env::var("ZIP_VIDEO_MPV_DIR") {
        cands.push(PathBuf::from(d));
    }
    cands.push(manifest.join("deps").join("mpv-dev"));
    for prof in ["debug", "release"] {
        cands.push(manifest.join("target").join(prof));
    }
    if let Ok(exe) = std::env::current_exe() {
        // target/debug/smoke_setup.exe → target/debug（build.rs 已把 dll 复制到这）
        if let Some(d) = exe.parent() {
            cands.push(d.to_path_buf());
        }
    }
    cands
        .into_iter()
        .map(|d| d.join("libmpv-2.dll"))
        .find(|p| p.exists())
}

/// WinRAR rar.exe 定位：ARCHVIEW_TEST_RAR → Program Files 标准位置
/// → 注册表卸载信息 InstallLocation（非标准盘符安装，如 D:\software\winrar）。
/// 注册表查询用 reg.exe 子进程（零依赖，reg 输出格式跨语言稳定）
fn find_rar_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ARCHVIEW_TEST_RAR") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    for base in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(base) {
            let p = PathBuf::from(pf).join("WinRAR").join("rar.exe");
            if p.exists() {
                return Some(p);
            }
        }
    }
    // 枚举 HKLM 卸载项，找 DisplayName 含 WinRAR 的 InstallLocation
    const UNINST: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINST_WOW: &str = r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    for key in [UNINST, UNINST_WOW] {
        let Ok(out) = Command::new("reg").args(["query", key, "/s"]).output() else {
            continue;
        };
        let mut display_hit = false;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            if line.starts_with("HKEY_") {
                display_hit = false; // 新子键块复位
            } else if let Some((name, data)) = parse_reg_sz(line) {
                match name {
                    "DisplayName" => display_hit = data.contains("WinRAR"),
                    "InstallLocation" if display_hit => {
                        let p = PathBuf::from(&data).join("rar.exe");
                        if p.exists() {
                            return Some(p);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

/// 解析 `reg query` 单行 `名    REG_SZ    数据`（数据可含空格，取 REG_SZ 之后原文）
fn parse_reg_sz(line: &str) -> Option<(&str, String)> {
    let idx = line.find("REG_SZ")?;
    let name = line[..idx].trim();
    let data = line[idx + "REG_SZ".len()..].trim();
    (!name.is_empty() && !data.is_empty()).then_some((name, data.to_owned()))
}

/// TOML literal string（单引号，反斜杠不转义，Windows 路径友好）
fn toml_lit(p: &Path) -> String {
    format!("'{}'", p.display())
}

/// 硬链接优先（同卷零拷贝），跨卷失败降级复制
fn place_dll(family_dir: &Path, mpv_dll: &Path) {
    std::fs::create_dir_all(family_dir).unwrap();
    let dst = family_dir.join("libmpv-2.dll");
    if !dst.exists() {
        if std::fs::hard_link(mpv_dll, &dst).is_err() {
            std::fs::copy(mpv_dll, &dst).unwrap();
        }
    }
}

// ---- 夹具族 ----

/// player_e2e：clip.mp4（3s testsrc2 320x180，hue 随机）+ test.zip（两 entry Deflated）+ DLL
fn gen_player(root: &Path, ffmpeg: &Path, mpv_dll: &Path, rng: &mut Rng) {
    let dir = root.join("player_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    place_dll(&dir, mpv_dll);

    let mp4 = dir.join("clip.mp4");
    let vf = hue_vf(rng);
    // 3 秒 320x180 testsrc2，超快速编码，画面色调每次随机（断言不含内容）
    run_ffmpeg(
        ffmpeg,
        &[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=3:size=320x180:rate=15",
            "-vf",
            &vf,
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ],
        &mp4,
    );

    // 两 entry Deflated zip（entry2 = 同一段视频，用于播完切换场景）
    let data = std::fs::read(&mp4).unwrap();
    let f = std::fs::File::create(dir.join("test.zip")).unwrap();
    let mut w = zip::ZipWriter::new(f);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    w.start_file("clip.mp4", opts).unwrap();
    w.write_all(&data).unwrap();
    w.start_file("clip2.mp4", opts).unwrap();
    w.write_all(&data).unwrap();
    w.finish().unwrap();
    println!("[player_e2e] clip.mp4 + test.zip + libmpv-2.dll 就绪");
}

/// rar 族三包（payload 与 rar_vfs 测试断言逐字节一致）
fn gen_rar(root: &Path, rar: &Path) {
    let dir = root.join("rar");
    std::fs::create_dir_all(&dir).unwrap();
    let payload = dir.join("_payload");
    std::fs::create_dir_all(&payload).unwrap();

    // test.rar：单文件 a.bin = 50000 × 0x09（-ep 不存路径）
    let a = payload.join("a.bin");
    std::fs::write(&a, vec![9u8; 50_000]).unwrap();
    run_rar(rar, &dir.join("test.rar"), &["-ep"], &[&a], &payload);

    // plain.rar / solid.rar：3 × 30000 填充 7/8/9（solid 加 -s）
    let files: Vec<PathBuf> = [7u8, 8, 9]
        .iter()
        .enumerate()
        .map(|(i, fill)| {
            let p = payload.join(format!("f{i}.bin"));
            std::fs::write(&p, vec![*fill; 30_000]).unwrap();
            p
        })
        .collect();
    let refs: Vec<&Path> = files.iter().map(|p| p.as_path()).collect();
    run_rar(rar, &dir.join("plain.rar"), &["-ep"], &refs, &payload);
    run_rar(rar, &dir.join("solid.rar"), &["-ep", "-s"], &refs, &payload);

    let _ = std::fs::remove_dir_all(&payload);
    println!("[rar] test.rar + plain.rar + solid.rar 就绪");
}

fn run_rar(rar: &Path, arch: &Path, flags: &[&str], files: &[&Path], cwd: &Path) {
    let mut cmd = Command::new(rar);
    cmd.arg("a").args(flags).arg(arch);
    for f in files {
        cmd.arg(f);
    }
    cmd.current_dir(cwd);
    let st = cmd.output().unwrap();
    assert!(
        st.status.success(),
        "rar.exe 打包失败: {}",
        String::from_utf8_lossy(&st.stderr)
    );
}

/// img：anim2.webp —— 红色系/蓝色系 20x14 两帧 @100ms（VP8X ANIM + ANMF），
/// 色值在测试断言包络内随机（红帧 r>200/g<60/b<60，蓝帧对称），
/// 生成命令与 src/imgview.rs 测试注释一致
fn gen_img(root: &Path, ffmpeg: &Path, rng: &mut Rng) {
    let dir = root.join("img");
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join("_tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let f0 = tmp.join("f0.png");
    let f1 = tmp.join("f1.png");
    // 红色系：r 高、g/b 低；蓝色系对称（通道优势断言的包络内随机）；
    // 画布 20x14 是断言锚点，保持固定
    let red = format!(
        "color=c=0x{:02X}{:02X}{:02X}:s=20x14",
        rng.range(220, 256),
        rng.range(10, 50),
        rng.range(10, 50)
    );
    let blue = format!(
        "color=c=0x{:02X}{:02X}{:02X}:s=20x14",
        rng.range(10, 50),
        rng.range(10, 50),
        rng.range(220, 256)
    );
    run_ffmpeg(ffmpeg, &["-y", "-f", "lavfi", "-i", &red, "-frames:v", "1"], &f0);
    run_ffmpeg(ffmpeg, &["-y", "-f", "lavfi", "-i", &blue, "-frames:v", "1"], &f1);
    run_ffmpeg(
        ffmpeg,
        &["-y", "-framerate", "10", "-i", tmp.join("f%d.png").to_str().unwrap(), "-loop", "0"],
        &dir.join("anim2.webp"),
    );
    let _ = std::fs::remove_dir_all(&tmp);
    println!("[img] anim2.webp 就绪");
}

/// diag：两段动画 webp（名字沿用原 test_media 样本，断言只依赖可解码+条目名，
/// 画面 hue 每次随机）
fn gen_diag(root: &Path, ffmpeg: &Path, rng: &mut Rng) {
    let dir = root.join("diag");
    std::fs::create_dir_all(&dir).unwrap();
    let vf = hue_vf(rng);
    run_ffmpeg(
        ffmpeg,
        &[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=1:size=960x640:rate=15",
            "-vf",
            &vf,
            "-c:v",
            "libwebp",
            "-q:v",
            "85",
            "-loop",
            "0",
        ],
        &dir.join("641.webp"),
    );
    let vf = hue_vf(rng);
    run_ffmpeg(
        ffmpeg,
        &[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=1:size=640x480:rate=15",
            "-vf",
            &vf,
            "-c:v",
            "libwebp",
            "-q:v",
            "75",
            "-loop",
            "0",
        ],
        &dir.join("1000.webp"),
    );
    println!("[diag] 641.webp + 1000.webp 就绪");
}

/// vartidy：VarTidy CLI 冒烟源（吸收自原 examples/make_smoke_fixtures.rs）
fn gen_vartidy(root: &Path) {
    let dir = root.join("vartidy");
    std::fs::create_dir_all(&dir).unwrap();

    // ---- pack1.zip：分类场景全覆盖 ----
    {
        let f = std::fs::File::create(dir.join("pack1.zip")).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let alice_v1: &[u8] = &vec![b'A'; 2000];
        let alice_v2: &[u8] = &vec![b'B'; 3000];
        for (name, data) in [
            ("Alice.look1.1.var", alice_v1),             // 仓库已有（仓库去重）
            ("dir1/Alice.look1.2.var", alice_v2),        // 新增
            ("dir2/sub/Alice.look1.1.var", alice_v1),    // 同名同内容（包内重复）
            ("dir3/Bob.scene.2.var", &[b'X'; 1000][..]), // 包内同名第一份
            ("dir4/Bob.scene.2.var", &[b'Y'; 1500][..]), // 同名不同内容（CRC 冲突）
            ("readme.txt", b"junk"),                     // 非var
            ("Cara.bad.var", b"x"),                      // 三段：格式错误
            ("Cara.item.x.var", b"x"),                   // 版本号非数字
        ] {
            w.start_file(name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    // ---- pack2.zip：新作者 + 已有作者新版本 ----
    {
        let f = std::fs::File::create(dir.join("pack2.zip")).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in [
            ("Newbie.first.1.var", &[b'N'; 1200][..]),
            ("Old.pkg.2.var", &[b'O'; 900][..]),
        ] {
            w.start_file(name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    // ---- pack3_enc.zip：AES 加密（密码 smoke-pw-123，文件名明文可见）----
    {
        let f = std::fs::File::create(dir.join("pack3_enc.zip")).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .with_aes_encryption(zip::AesMode::Aes256, "smoke-pw-123");
        for (name, data) in [
            ("dir/EncGirl.look1.1.var", &[b'E'; 2048][..]),
            ("EncGuy.scene.3.var", &[b'G'; 1024][..]),
        ] {
            w.start_file(name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    // ---- pack6_sp.zip：AES 加密（特殊字符密码 sm0ke!@#2026）----
    {
        let f = std::fs::File::create(dir.join("pack6_sp.zip")).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .with_aes_encryption(zip::AesMode::Aes256, "sm0ke!@#2026");
        for (name, data) in [
            ("Sp.author.1.var", &[b'S'; 1792][..]),
            ("Sp.scene.9.var", &[b'P'; 896][..]),
        ] {
            w.start_file(name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    // ---- 7z 包：payload 临时目录（from_path 需要真实文件），完成后清理 ----
    let payload_dir = dir.join("_payload");
    std::fs::create_dir_all(&payload_dir).unwrap();
    let push_7z = |w: &mut sevenz_rust::SevenZWriter<std::fs::File>, file: &str, name: &str| {
        let p = payload_dir.join(file);
        std::fs::write(&p, vec![b'7'; 1536]).unwrap();
        let e = sevenz_rust::SevenZArchiveEntry::from_path(&p, name.into());
        w.push_archive_entry(e, Some(std::fs::File::open(&p).unwrap()))
            .unwrap();
    };

    // pack4.7z：明文，多级目录
    {
        let mut w = sevenz_rust::SevenZWriter::create(dir.join("pack4.7z")).unwrap();
        push_7z(&mut w, "a.bin", "Seven.man1.1.var");
        push_7z(&mut w, "b.bin", "sub/dir/Seven.deep.2.var");
        push_7z(&mut w, "c.bin", "note.txt");
        w.finish().unwrap();
    }
    // pack5_enc.7z：内容加密（密码 smoke-pw-123，文件名明文可见）
    {
        let mut w = sevenz_rust::SevenZWriter::create(dir.join("pack5_enc.7z")).unwrap();
        w.set_content_methods(vec![sevenz_rust::AesEncoderOptions::new(
            sevenz_rust::Password::from("smoke-pw-123"),
        )
        .into()]);
        push_7z(&mut w, "d.bin", "dir/Crypt.author.1.var");
        push_7z(&mut w, "e.bin", "Crypt.other.5.var");
        w.finish().unwrap();
    }
    let _ = std::fs::remove_dir_all(&payload_dir);

    // ---- var_repo 既有资产（整理器绝不允许修改）----
    for (rel, data) in [
        ("Alice/Alice.look1.1.var", &b"REPO-EXISTING-ALICE-V1"[..]),
        ("Old/Old.pkg.1.var", &b"REPO-EXISTING-OLD-PKG1"[..]),
    ] {
        let p = dir.join("var_repo").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    println!("[vartidy] pack1-6 + var_repo 就绪");
}

/// ffmpeg 封装：失败即 panic（夹具生成是 setup 的职责，失败不该被测试期吞掉）
fn run_ffmpeg(ffmpeg: &Path, args: &[&str], out: &Path) {
    let st = Command::new(ffmpeg).args(args).arg(out).output().unwrap();
    assert!(
        st.status.success(),
        "ffmpeg 生成 {} 失败: {}",
        out.display(),
        String::from_utf8_lossy(&st.stderr)
    );
}
