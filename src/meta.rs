//! 图片元数据（EXIF）：方向/评级读取、旋转/评级写回。
//!
//! 读 = kamadak-exif（支持内存 buffer，zip 流路径必需）；
//! 写 = little_exif（纯 Rust）。**PNG 例外**：库写 PNG 只产 zTXt
//! "Raw profile type exif"（ImageMagick 旧形态，Windows/PNG 标准/
//! kamadak 都不认），故 PNG 由本模块自写标准 eXIf 块注入（tag 编码
//! 仍用 little_exif 的 encode()，见 write_png_orientation）。
//!
//! 设计约束（PLAN_image_meta.md §6）：
//! - 文件元数据是唯一真相；写回只动元数据段，不改像素；
//! - 写失败不动文件（调用方 set_err 提示）；
//! - 评级真相链（2026-11-22 定版，sidecar 优先）：**写入只走 sidecar
//!   `.xmp`**（原文件字节零改动；EXIF 18246/18249 是 Microsoft 非正式扩展
//!   位，主流 Lightroom/digiKam 写 XMP，不再往 EXIF 写）；**读取**按
//!   sidecar > 嵌入式 XMP（xmp:Rating，其它主流软件写）> EXIF 18246/18249
//!   （向下兼容存量照片）三级优先。

use image::DynamicImage;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// 主流可写回元数据的图片扩展名（小写）。方向/评级值均用裸 u8 表达
///（Orientation 有效域 1-8）。GIF/BMP 无 EXIF 容器；
/// WebP 的 lossy 变体 little_exif 不支持，由运行时报错兜底（调用方
/// 捕获后走"会话内"分支）。
pub fn meta_writable(name: &str) -> bool {
    const EXTS: &[&str] = &["jpg", "jpeg", "jpe", "png", "tif", "tiff", "webp"];
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// EXIF 元数据摘要（解码链 / 后台扫描共用）
#[derive(Debug, Clone, Copy, Default)]
pub struct ExifMeta {
    /// Orientation tag 值（1-8；无 tag 或畸形 = 1）
    pub orientation: u8,
    /// 评级 0-5；None = 无评级记录（tag 18246 值 0 视为未评）
    pub rating: Option<u8>,
}

/// 从内存 buffer 读 EXIF（zip 流路径：条目已全量读入，零额外 IO）
pub fn read_meta_bytes(buf: &[u8]) -> Option<ExifMeta> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(buf))
        .ok()?;
    Some(parse_exif(&exif))
}

/// 从文件读 EXIF（目录模式：解码链 File 源 / 后台扫描）
pub fn read_meta_path(path: &Path) -> Option<ExifMeta> {
    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()?;
    Some(parse_exif(&exif))
}

fn parse_exif(exif: &exif::Exif) -> ExifMeta {
    let orientation = exif
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v.clamp(1, 8) as u8)
        .unwrap_or(1);
    // 评级：优先 tag 18246（0-5 星，Windows 主写）；缺失/0 时回退
    // RatingPercent 18249（1/25/50/75/99 ↔ 1-5 星，同写对出现）
    let rating = exif
        .get_field(exif::Tag(exif::Context::Tiff, 18246), exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v.min(5) as u8)
        .filter(|&r| r > 0)
        .or_else(|| {
            exif.get_field(exif::Tag(exif::Context::Tiff, 18249), exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
                .and_then(percent_to_rating)
        });
    ExifMeta {
        orientation,
        rating,
    }
}

/// RatingPercent → 星级：1/25/50/75/99 ↔ 1-5（Windows 写法），其余值四舍五入
fn percent_to_rating(pct: u32) -> Option<u8> {
    let r = ((pct as f64 / 20.0).round() as u32).clamp(1, 5) as u8;
    Some(r)
}

/// 嵌入式 XMP 的 xmp:Rating 搜索标记（Lightroom/digiKam 等主流软件写入
/// JPEG/PNG 的 XMP 包内；仅文件头部出现，读前 512KB 搜索即可）
const XMP_RATING_MARK: &str = "xmp:Rating";

/// 评级 sidecar 路径（Lightroom 约定）：`IMG_1234.jpg` → `IMG_1234.xmp`
///（with_extension 替换最后一段扩展名；无扩展名直接追加）。
/// sidecar 完全不碰原始文件字节，且对 GIF/BMP/RAW 全格式统一可用
pub fn sidecar_path(image_path: &Path) -> PathBuf {
    image_path.with_extension("xmp")
}

/// 从 sidecar `.xmp` 文本解析 xmp:Rating（0-5）。兼容两种 XMP 形态：
/// 元素式 `<xmp:Rating>3</xmp:Rating>`（我们/Lightroom 写法）与
/// 属性式 `xmp:Rating="3"`；小数评级（2.33）四舍五入。无标记 = None
pub fn read_sidecar_rating(path: &Path) -> Option<u8> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_xmp_rating(&text)
}

/// 在文本（sidecar 全文 / 嵌入式 XMP 包前缀）中解析 xmp:Rating。
/// 逐处扫描标记出现点：元素式紧随 `>` 取到下一个 `<`；属性式
/// `xmp:Rating="3"`/`'3'`（允许空白）。首个带合法数值的命中即返回
fn parse_xmp_rating(text: &str) -> Option<u8> {
    let mut rest = text;
    while let Some((_, after)) = rest.split_once(XMP_RATING_MARK) {
        // 元素式：<xmp:Rating>3</xmp:Rating>
        if let Some(v) = after.strip_prefix('>') {
            if let Some(end) = v.find('<') {
                if let Ok(x) = v[..end].trim().parse::<f64>() {
                    return Some((x.round() as u8).min(5));
                }
            }
        }
        // 属性式：xmp:Rating = "3" / '3'
        let a = after.trim_start();
        if let Some(v) = a.strip_prefix('=').map(str::trim_start) {
            for q in ['"', '\''] {
                if let Some(v) = v.strip_prefix(q) {
                    if let Some(end) = v.find(q) {
                        if let Ok(x) = v[..end].trim().parse::<f64>() {
                            return Some((x.round() as u8).min(5));
                        }
                    }
                    break;
                }
            }
        }
        rest = after; // 该处无合法值（如闭标签/声明）→ 继续下一处
    }
    None
}

/// 把评级写入 sidecar `.xmp`（XMP 标准数据包模板，xmp 命名空间 =
/// http://ns.adobe.com/xap/1.0/）。r=0 = 删除 sidecar（清除语义，
/// 不留 Rating=0 的孤儿文件）。原图片文件字节零改动
pub fn write_sidecar_rating(image_path: &Path, r: u8) -> Result<(), String> {
    let sidecar = sidecar_path(image_path);
    let r = r.min(5);
    if r == 0 {
        match std::fs::remove_file(&sidecar) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("删除 sidecar 失败: {e}")),
        }
    } else {
        std::fs::write(&sidecar, xmp_packet(r)).map_err(|e| format!("写入 sidecar 失败: {e}"))
    }
}

/// 最小 XMP 数据包（xpacket 包裹 + rdf:Description 展开元素式 Rating）；
/// begin 处为 BOM 字符（XMP 规范要求 UTF-8 BOM 占位）
fn xmp_packet(rating: u8) -> String {
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\n\
         <xmp:Rating>{rating}</xmp:Rating>\n\
         </rdf:Description>\n\
         </rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>"
    )
}

/// 从文件读嵌入式 XMP 头部并解析 xmp:Rating（其它主流软件写进文件内的
/// 评级；读前 512KB 搜索——XMP 段在 JPEG APP1/PNG iTXt 均位于文件头部）。
/// 无标记/读取失败 = None（不报错，扫描线程容忍）
pub fn read_embedded_xmp_rating(path: &Path) -> Option<u8> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 512 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);
    parse_xmp_rating(&text)
}

/// 方向链旋转映射：当前 tag `o` 的显示效果顺/逆时针转 90° 后应写的新 tag 值。
/// 推导：T(o') = Rot ∘ T(o)（位图不动、只改 tag，显示 = T(o) 变换）。
/// 顺时针：1→6, 2→7, 3→8, 4→5, 5→2, 6→3, 7→4, 8→1
pub fn orientation_rotate(o: u8, cw: bool) -> u8 {
    const CW: [u8; 9] = [1, 6, 7, 8, 5, 2, 3, 4, 1];
    const CCW: [u8; 9] = [1, 8, 5, 6, 7, 4, 1, 2, 3];
    let idx = o.clamp(1, 8) as usize;
    if cw {
        CW[idx]
    } else {
        CCW[idx]
    }
}

/// EXIF 方向矫正：对解码后的位图应用 T(o) 变换得到正立显示图。
/// o 取值 1-8，穷尽 EXIF 定义；非法值按 1（不变换）处理。
/// 镜像/180° 用 in_place 零分配；90/270 旋转需重排行列返回新图；
/// EXIF 5/7 = 旋转后左右镜像（主/反对角线转置）
pub fn apply_orientation(img: DynamicImage, o: u8) -> DynamicImage {
    use image::imageops;
    match o {
        1 => img,
        2 => {
            let mut i = img;
            imageops::flip_horizontal_in_place(&mut i);
            i
        }
        3 => {
            let mut i = img;
            imageops::rotate180_in_place(&mut i);
            i
        }
        4 => {
            let mut i = img;
            imageops::flip_vertical_in_place(&mut i);
            i
        }
        // EXIF 5：顺时针 90° 后左右镜像（主对角线转置）。
        // image 0.25 的 imageops 旋转/镜像自由函数返回 ImageBuffer
        //（非 I），统一 to_rgba8 再包回 DynamicImage
        5 => DynamicImage::ImageRgba8(imageops::flip_horizontal(&imageops::rotate90(
            &img.to_rgba8(),
        ))),
        // EXIF 6：顺时针 90°
        6 => DynamicImage::ImageRgba8(imageops::rotate90(&img.to_rgba8())),
        // EXIF 7：顺时针 270° 后左右镜像（反对角线转置）
        7 => DynamicImage::ImageRgba8(imageops::flip_horizontal(&imageops::rotate270(
            &img.to_rgba8(),
        ))),
        // EXIF 8：顺时针 270°
        8 => DynamicImage::ImageRgba8(imageops::rotate270(&img.to_rgba8())),
        _ => img,
    }
}

/// 把 Orientation 写回图片文件（保留其余 EXIF tag）。o 必须为 1-8。
/// 注意：文件可能尚无任何元数据段（如 image crate 产出的 PNG 无 eXIf
/// 块），little_exif 对此报 "No metadata found!"（与其文档注释"解码失败
/// 返回空对象"不符，实测踩中）→ 回退空 Metadata 起步；若是权限等真实
/// IO 问题，后续 write_to_file 读文件时会以更准确的错误再次失败
pub fn write_orientation(path: &Path, o: u8) -> Result<(), String> {
    // PNG 分流：little_exif 0.6.23 写 PNG 只产 zTXt "Raw profile type exif"
    //（ImageMagick 旧形态），而 PNG ext 1.5 标准 / Windows / kamadak-exif
    //（我们的读链）都只认 eXIf 块——库读写不对称，写出的方向谁也读不到
    //（重开 app 方向即丢失，实测踩中）。改为自己注入标准 eXIf 块，
    // tag 内容仍由 little_exif 编码（encode() 输出 = 标准 TIFF blob）
    let is_png = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    if is_png {
        return write_png_orientation(path, o);
    }

    let mut md = match little_exif::metadata::Metadata::new_from_path(path) {
        Ok(md) => md,
        Err(_) => little_exif::metadata::Metadata::new(),
    };
    md.set_tag(little_exif::exif_tag::ExifTag::Orientation(vec![o as u16]));
    md.write_to_file(path)
        .map_err(|e| format!("写入元数据失败: {e}"))
}

/// PNG 方向写回：little_exif 读出完整元数据（其读链支持 eXIf 块与
/// zTXt raw profile 两形态）→ set_tag → encode() 产出标准 TIFF blob
/// → 自行做 PNG chunk 级替换（旧 eXIf 丢弃，新块插在 IHDR 之后——
/// PNG ext 1.5 要求 eXIf 位于 IHDR 后、首个 IDAT 前）。其余 EXIF tag
/// 经 Metadata 全量保留
fn write_png_orientation(path: &Path, o: u8) -> Result<(), String> {
    let mut md = match little_exif::metadata::Metadata::new_from_path(path) {
        Ok(md) => md,
        Err(_) => little_exif::metadata::Metadata::new(),
    };
    md.set_tag(little_exif::exif_tag::ExifTag::Orientation(vec![o as u16]));
    let tiff = md.encode().map_err(|e| format!("编码 EXIF 失败: {e}"))?;
    png_replace_exif_chunk(path, &tiff)
}

/// PNG chunk 级替换：移除全部既有 eXIf 块，在 IHDR 之后插入新 eXIf 块。
/// chunk 布局 = len(4) + type(4) + data(len) + crc(4)；CRC32 按
/// type+data 计算（ISO HDLC，PNG 标准多项式）
fn png_replace_exif_chunk(path: &Path, payload: &[u8]) -> Result<(), String> {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let data = std::fs::read(path).map_err(|e| format!("读取 PNG 失败: {e}"))?;
    if !data.starts_with(&PNG_SIG) {
        return Err("不是有效 PNG 文件".to_string());
    }

    let mut out: Vec<u8> = Vec::with_capacity(data.len() + payload.len() + 12);
    out.extend_from_slice(&PNG_SIG);
    let mut pos = 8usize;
    let mut inserted = false;
    while pos + 12 <= data.len() {
        let len = u32::from_be_bytes([
            data[pos],
            data[pos + 1],
            data[pos + 2],
            data[pos + 3],
        ]) as usize;
        let chunk_end = pos + 12 + len; // data(len) + crc(4)
        if chunk_end > data.len() {
            return Err("PNG chunk 布局损坏".to_string());
        }
        let ctype = &data[pos + 4..pos + 8];
        if ctype == b"IHDR" {
            out.extend_from_slice(&data[pos..chunk_end]);
            // eXIf 紧跟 IHDR（PNG ext 1.5 合法位置：IHDR 后、首个 IDAT 前）
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            out.extend_from_slice(b"eXIf");
            out.extend_from_slice(payload);
            let mut h = crc32fast::Hasher::new();
            h.update(b"eXIf");
            h.update(payload);
            out.extend_from_slice(&h.finalize().to_be_bytes());
            inserted = true;
        } else if ctype == b"eXIf" {
            // 旧 eXIf 丢弃：新块经 Metadata 全量编码已含其余 tag
        } else {
            out.extend_from_slice(&data[pos..chunk_end]);
        }
        pos = chunk_end;
    }
    if !inserted {
        return Err("PNG 缺少 IHDR 块".to_string());
    }
    std::fs::write(path, out).map_err(|e| format!("写入 PNG 失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旋转映射全矩阵：任一 o 双向轮转 4 次必回原值；cw/ccw 互逆
    #[test]
    fn orientation_rotate_全矩阵轮转回原值() {
        for o in 1..=8u8 {
            let mut cw = o;
            let mut ccw = o;
            for _ in 0..4 {
                cw = orientation_rotate(cw, true);
                ccw = orientation_rotate(ccw, false);
            }
            assert_eq!(cw, o, "cw 4 次回原值失败 o={o}");
            assert_eq!(ccw, o, "ccw 4 次回原值失败 o={o}");
        }
        // cw∘ccw = 恒等
        for o in 1..=8u8 {
            assert_eq!(orientation_rotate(orientation_rotate(o, true), false), o);
        }
        // 抽查已知点：正立图顺时针转 90° → tag 6（"顺时针 90° 显示"）
        assert_eq!(orientation_rotate(1, true), 6);
        assert_eq!(orientation_rotate(6, true), 3);
        assert_eq!(orientation_rotate(3, true), 8);
        assert_eq!(orientation_rotate(8, true), 1);
    }

    /// apply_orientation 与旋转映射的自洽性：tag o 的图顺时针转 90° 等价
    /// 于位图不变、tag 变 orientation_rotate(o, true)——用旋转序列判定：
    /// T(o) 再顺时针 90° 的复合效果 = T(new_o)。以正方形网格位图逐点核对
    #[test]
    fn apply_orientation_与旋转映射自洽() {
        // 构造 3x3 渐变图，逐 o 验证 T(o) 后再 rotate90 == T(cw(o))
        let img = gradient3x3();
        for o in 1..=8u8 {
            let a = rotate_cw_bit(&apply_orientation(img.clone(), o));
            let b = apply_orientation(img.clone(), orientation_rotate(o, true));
            assert_pixels_eq(&a, &b, o);
        }
    }

    /// 8 值方向映射逐值核对（3x3 渐变图旋转 90° 的坐标变换）
    #[test]
    fn apply_orientation_八个exif方向映射() {
        let img = gradient3x3();
        // EXIF 6（顺时针 90°）：原 (0,0)=1 → (2,0)
        let r6 = apply_orientation(img.clone(), 6);
        assert_eq!(get_px(&r6, 2, 0), 1);
        assert_eq!(get_px(&r6, 0, 0), 7);
        // EXIF 8（逆时针 90°）：原 (0,0)=1 → (0,2)
        let r8 = apply_orientation(img.clone(), 8);
        assert_eq!(get_px(&r8, 0, 2), 1);
        // EXIF 3（180°）：原 (0,0)=1 → (2,2)
        let r3 = apply_orientation(img.clone(), 3);
        assert_eq!(get_px(&r3, 2, 2), 1);
        // EXIF 2（水平镜像）：原 (0,0)=1 → (2,0)
        let r2 = apply_orientation(img.clone(), 2);
        assert_eq!(get_px(&r2, 2, 0), 1);
        // EXIF 4（垂直镜像）：原 (0,0)=1 → (0,2)
        let r4 = apply_orientation(img.clone(), 4);
        assert_eq!(get_px(&r4, 0, 2), 1);
    }

    /// 3x3 灰度渐变图：像素值 = y*3 + x + 1
    fn gradient3x3() -> DynamicImage {
        let mut buf = Vec::with_capacity(9);
        for y in 0..3u8 {
            for x in 0..3u8 {
                buf.push(y * 3 + x + 1);
            }
        }
        DynamicImage::ImageLuma8(
            image::GrayImage::from_raw(3, 3, buf).expect("9 bytes for 3x3"),
        )
    }

    /// 位图顺时针 90°（像素级，与 imageops::rotate90 同语义）
    fn rotate_cw_bit(img: &DynamicImage) -> DynamicImage {
        DynamicImage::ImageLuma8(image::imageops::rotate90(&img.to_luma8()))
    }

    fn get_px(img: &DynamicImage, x: u32, y: u32) -> u8 {
        img.to_luma8().get_pixel(x, y)[0]
    }

    fn assert_pixels_eq(a: &DynamicImage, b: &DynamicImage, o: u8) {
        let a = a.to_luma8();
        let b = b.to_luma8();
        assert_eq!(a.dimensions(), b.dimensions(), "尺寸不等 o={o}");
        for (pa, pb) in a.pixels().zip(b.pixels()) {
            assert_eq!(pa[0], pb[0], "像素不等 o={o} at {:?} {:?}", pa, pb);
        }
    }

    /// sidecar 命名（Lightroom 约定）：替换最后一段扩展名
    #[test]
    fn sidecar_path_命名约定() {
        assert_eq!(
            sidecar_path(Path::new("d/IMG_1234.jpg")),
            Path::new("d/IMG_1234.xmp")
        );
        assert_eq!(
            sidecar_path(Path::new("photo.tar.gz")), // 只替换最后一段
            Path::new("photo.tar.xmp")
        );
        assert_eq!(sidecar_path(Path::new("noext")), Path::new("noext.xmp"));
    }

    /// parse_xmp_rating：元素式/属性式/小数取整/无标记 None/闭标签不误判
    #[test]
    fn parse_xmp_rating_各形态() {
        let packet = xmp_packet(3);
        assert_eq!(parse_xmp_rating(&packet), Some(3));
        assert_eq!(
            parse_xmp_rating("<x:xmpmeta><xmp:Rating>5</xmp:Rating></x:xmpmeta>"),
            Some(5)
        );
        assert_eq!(
            parse_xmp_rating("<rdf:Description xmp:Rating=\"2\"/>"),
            Some(2)
        );
        assert_eq!(
            parse_xmp_rating("<rdf:Description xmp:Rating = '4' />"),
            Some(4)
        );
        // Lightroom 小数评级四舍五入
        assert_eq!(
            parse_xmp_rating("<xmp:Rating>2.33</xmp:Rating>"),
            Some(2)
        );
        assert_eq!(parse_xmp_rating("<xmp:Rating>4.6</xmp:Rating>"), Some(5));
        // 超界钳制
        assert_eq!(
            parse_xmp_rating("<xmp:Rating>9</xmp:Rating>"),
            Some(5)
        );
        // 无标记 / 闭标签在前不误判
        assert_eq!(parse_xmp_rating("no rating here"), None);
        assert_eq!(
            parse_xmp_rating("</xmp:Rating></x:xmpmeta>"),
            None
        );
    }

    /// write/read_sidecar_rating 往返 + r=0 清除（真实临时文件）
    #[test]
    fn sidecar_写入读取清除往返() {
        let dir = std::env::temp_dir().join(format!("archview_meta_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("t1.jpg");
        let side = sidecar_path(&img);
        let _ = std::fs::remove_file(&side);
        // 写 5 → 读回 5 → 原图片文件不存在也能写（sidecar 独立）
        write_sidecar_rating(&img, 5).unwrap();
        assert_eq!(read_sidecar_rating(&side), Some(5));
        // 清除 → sidecar 删除 → 读 None；重复清除幂等
        write_sidecar_rating(&img, 0).unwrap();
        assert!(!side.exists());
        assert_eq!(read_sidecar_rating(&side), None);
        write_sidecar_rating(&img, 0).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 全 meta_writable 格式矩阵诊断：各格式建文件 → write_orientation →
    /// kamadak-exif 读回（与 app 解码链同源）。打印每格式的写/读结果，
    /// 断言全部 Some(6)。含 PNG 自写 eXIf 块注入链路
    #[test]
    fn 全格式方向写回往返诊断() {
        let cases: &[(&str, image::ImageFormat)] = &[
            ("jpg", image::ImageFormat::Jpeg),
            ("jpeg", image::ImageFormat::Jpeg),
            ("png", image::ImageFormat::Png),
            ("tif", image::ImageFormat::Tiff),
            ("webp", image::ImageFormat::WebP),
        ];
        for (ext, fmt) in cases {
            let back = 写回往返诊断(ext, *fmt);
            println!("{ext}: 读回 orientation={back:?}");
            assert_eq!(back, Some(3), "{ext} 方向写回后读回失败");
        }
    }

    /// 诊断共用：独立临时目录（不得与 sidecar 测试共用——其收尾
    /// remove_dir_all 会删掉并行测试的文件，实测踩中）建 {ext} 文件
    ///（image crate 产出、无 EXIF 段）→ write_orientation(6) 读回 →
    /// write_orientation(3) 读回（二次写回覆盖既有 EXIF——PNG 走
    /// eXIf 替换分支）→ 解码验证 → 返回终值
    fn 写回往返诊断(ext: &str, fmt: image::ImageFormat) -> Option<u8> {
        let dir = std::env::temp_dir().join(format!("archview_meta_test_diag_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("t_diag.{ext}"));
        let _ = std::fs::remove_file(&path);
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut std::fs::File::create(&path).unwrap(), fmt)
            .unwrap();

        let res = write_orientation(&path, 6);
        if let Err(e) = &res {
            eprintln!("write_orientation {ext} 错误: {e}");
        }
        assert!(res.is_ok(), "{ext} 写入失败");
        let back = read_meta_path(&path).map(|m| m.orientation);
        assert_eq!(back, Some(6), "{ext} 首次写回读回失败");
        // 二次写回（覆盖已有 EXIF 的文件——PNG 走 eXIf 替换分支）+ 图片
        // 数据完整性（chunk 注入/替换不得破坏解码）
        write_orientation(&path, 3).unwrap();
        let back2 = read_meta_path(&path).map(|m| m.orientation);
        assert_eq!(back2, Some(3), "{ext} 二次写回读回失败");
        image::open(&path).unwrap_or_else(|e| panic!("{ext} 写回后解码失败: {e}"));
        back2
    }
}
