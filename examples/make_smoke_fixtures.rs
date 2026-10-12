//! 生成 VarTidy 冒烟测试源（test_tidy/，产物已 gitignore）。
//! 用法：cargo run --example make_smoke_fixtures
//!
//! 生成内容：
//!   pack1.zip      分类场景全覆盖（合法/包内重复/同名不同CRC/五类非法）
//!   pack2.zip      新作者 + 已有作者的新版本
//!   pack3_enc.zip  AES 加密 var 包（密码 smoke-pw-123）
//!   pack4.7z       7z 明文包（多级目录）
//!   pack5_enc.7z   7z 内容加密包（密码 smoke-pw-123）
//!   pack6_sp.zip   AES 加密 var 包（特殊字符密码 sm0ke!@#2026）
//!   var_repo/      仓库既有资产（Alice.look1.1.var / Old.pkg.1.var）

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_tidy");
    std::fs::create_dir_all(&root).unwrap();
    // 幂等：只清理生成物（本目录下的 setup.bat 等脚本保留——
    // remove_dir_all 整目录会删掉正在执行的 setup.bat，导致 "batch file cannot be found"）
    for entry in std::fs::read_dir(&root).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_generated = [
            "pack1.zip",
            "pack2.zip",
            "pack3_enc.zip",
            "pack4.7z",
            "pack5_enc.7z",
            "pack6_sp.zip",
        ]
        .contains(&name.as_str())
            || name == "var_repo"
            || name == "_payload"
            || name.starts_with("vartidy_report_");
        if is_generated {
            let p = entry.path();
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(p);
            } else {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    // ---- pack1.zip：分类场景全覆盖 ----
    {
        let f = std::fs::File::create(root.join("pack1.zip")).unwrap();
        let mut w = zip::write::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let alice_v1: &[u8] = &vec![b'A'; 2000];
        let alice_v2: &[u8] = &vec![b'B'; 3000];
        for (name, data) in [
            ("Alice.look1.1.var", alice_v1),      // 仓库已有（仓库去重）
            ("dir1/Alice.look1.2.var", alice_v2), // 新增
            ("dir2/sub/Alice.look1.1.var", alice_v1), // 同名同内容（包内重复）
            ("dir3/Bob.scene.2.var", &[b'X'; 1000][..]), // 包内同名第一份
            ("dir4/Bob.scene.2.var", &[b'Y'; 1500][..]), // 同名不同内容（CRC 冲突）
            ("readme.txt", b"junk"),              // 非var
            ("Cara.bad.var", b"x"),               // 三段：格式错误
            ("Cara.item.x.var", b"x"),            // 版本号非数字
        ] {
            w.start_file(name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    // ---- pack2.zip：新作者 + 已有作者新版本 ----
    {
        let f = std::fs::File::create(root.join("pack2.zip")).unwrap();
        let mut w = zip::write::ZipWriter::new(f);
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
        let f = std::fs::File::create(root.join("pack3_enc.zip")).unwrap();
        let mut w = zip::write::ZipWriter::new(f);
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
        let f = std::fs::File::create(root.join("pack6_sp.zip")).unwrap();
        let mut w = zip::write::ZipWriter::new(f);
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
    let payload_dir = root.join("_payload");
    std::fs::create_dir_all(&payload_dir).unwrap();
    let push_7z = |w: &mut sevenz_rust::SevenZWriter<std::fs::File>,
                   file: &str,
                   name: &str| {
        let p = payload_dir.join(file);
        std::fs::write(&p, vec![b'7'; 1536]).unwrap();
        let e = sevenz_rust::SevenZArchiveEntry::from_path(&p, name.into());
        w.push_archive_entry(e, Some(std::fs::File::open(&p).unwrap())).unwrap();
    };

    // pack4.7z：明文，多级目录
    {
        let mut w = sevenz_rust::SevenZWriter::create(root.join("pack4.7z")).unwrap();
        push_7z(&mut w, "a.bin", "Seven.man1.1.var");
        push_7z(&mut w, "b.bin", "sub/dir/Seven.deep.2.var");
        push_7z(&mut w, "c.bin", "note.txt");
        w.finish().unwrap();
    }
    // pack5_enc.7z：内容加密（密码 smoke-pw-123，文件名明文可见）
    {
        let mut w = sevenz_rust::SevenZWriter::create(root.join("pack5_enc.7z")).unwrap();
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
        let p = root.join("var_repo").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    println!("测试源已生成: {}", root.display());
    println!("冒烟: test_tidy\\setup.bat（或手动执行 archview tidy）");
}
