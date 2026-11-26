//! 图片元数据：EXIF 方向读取/写回、图片评级（分格式真相矩阵）。
//!
//! 方向：读 = kamadak-exif（支持内存 buffer，zip 流路径必需）；
//! 写 = little_exif（纯 Rust）。**PNG 例外**：库写 PNG 只产 zTXt
//! "Raw profile type exif"（ImageMagick 旧形态，Windows/PNG 标准/
//! kamadak 都不认），故 PNG 由本模块经 img-parts 注入标准 eXIf 块
//!（tag 编码仍用 little_exif 的 encode()，见 write_png_orientation）。
//!
//! 评级（2026-11-23 定版，**分格式真相矩阵，无跨格式兜底**）：
//! - jpg/jpeg/jpe/png/webp → 嵌入式 XMP（xmp:Rating）。img-parts 容器
//!   手术（JPEG APP1 / PNG iTXt / WebP "XMP " 块）+ XMP 文本编辑：
//!   已有包原位替换优先，无标记则在 `</rdf:RDF>` 前追加独立
//!   rdf:Description（不做属性合并）；清除 = 剥离 Rating 保留其余内容。
//!   WebP 简单格式（无 VP8X 块）无元数据容器，显式 Err 不静默兜底
//!   （img-parts 内部 convert_into_infered_kind 其实能自动补 VP8X，
//!   但只能经 set_icc_profile/set_exif 等副作用路径触发，不干净不用）
//! - tif/tiff + 相机 RAW（arw/nef/nrw/srf/sr2/arq）+ bmp + gif →
//!   sidecar `.xmp`（Lightroom 命名约定，原文件字节零改动）
//! - EXIF 18246/18249 评级读取已删除（Microsoft 非正式扩展位，主流
//!   Lightroom/digiKam 只写 XMP，存量 EXIF 评级不再认）；压缩包内
//!   不探测不写入（调用方保证）
//! - 写失败不动文件（调用方 set_err 提示）；评级延迟批量写由上层
//!   管理（dirty 集 + 同步 worker），本模块只提供单文件读写原语

use image::DynamicImage;
use img_parts::jpeg::{markers, Jpeg, JpegSegment};
use img_parts::png::{Png, PngChunk};
use img_parts::riff::{RiffChunk, RiffContent};
use img_parts::vp8::VP8Kind;
use img_parts::webp::{WebP, CHUNK_XMP};
use img_parts::ImageEXIF;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// 主流可写回元数据的图片扩展名（小写）。方向值用裸 u8 表达
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

/// EXIF 元数据摘要（解码链用：方向矫正）
#[derive(Debug, Clone, Copy, Default)]
pub struct ExifMeta {
    /// Orientation tag 值（1-8；无 tag 或畸形 = 1）
    pub orientation: u8,
}

/// 从内存 buffer 读 EXIF（zip 流路径：条目已全量读入，零额外 IO）
pub fn read_meta_bytes(buf: &[u8]) -> Option<ExifMeta> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(buf))
        .ok()?;
    Some(parse_exif(&exif))
}

/// 从文件读 EXIF。生产解码链已全量预读走 [`read_meta_bytes`]（一次 IO 拿全，
/// 不再单独开文件读头）；本函数现仅测试使用（临时文件读回验证）
#[cfg(test)]
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
    ExifMeta { orientation }
}

// ---------------------------------------------------------------------------
// 评级：分格式真相矩阵
// ---------------------------------------------------------------------------

/// 评级的存储形态（分格式矩阵，无跨格式兜底）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatingKind {
    /// 嵌入式 XMP（xmp:Rating）：jpg/jpeg/jpe/png/webp
    Embedded,
    /// sidecar `.xmp`：tif/tiff + 相机 RAW + bmp + gif
    Sidecar,
}

/// 按文件名判定评级存储形态。不在矩阵名单 = None（不支持评级：
/// 压缩包外只可能是非图片或 demux raw）
pub fn rating_kind(name: &str) -> Option<RatingKind> {
    const EMBEDDED: &[&str] = &["jpg", "jpeg", "jpe", "png", "webp"];
    const SIDECAR: &[&str] = &["tif", "tiff", "arw", "nef", "nrw", "srf", "sr2", "arq", "bmp", "gif"];
    let e = Path::new(name).extension().and_then(|e| e.to_str())?;
    if EMBEDDED.iter().any(|x| x.eq_ignore_ascii_case(e)) {
        Some(RatingKind::Embedded)
    } else if SIDECAR.iter().any(|x| x.eq_ignore_ascii_case(e)) {
        Some(RatingKind::Sidecar)
    } else {
        None
    }
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

/// 读图片评级（后台扫描/同步用）。按矩阵分格式：
/// Embedded = 头部 ≤512KB 文本搜索 xmp:Rating（XMP 包在 JPEG APP1 /
/// PNG iTXt / WebP XMP 块，均位于文件头部）；Sidecar = 读 sidecar。
/// 无标记/读取失败 = None（不报错，扫描线程容忍）
pub fn read_rating(path: &Path, name: &str) -> Option<u8> {
    match rating_kind(name)? {
        RatingKind::Sidecar => read_sidecar_rating(&sidecar_path(path)),
        RatingKind::Embedded => read_embedded_xmp(&read_head_bytes(path)?),
    }
}

/// 写图片评级。r=0 = 清除（sidecar 删文件 / 嵌入式剥离 Rating 保留
/// 其余内容）。调用方（同步 worker）负责先检查文件存在性——本函数
/// 保持严格：文件缺失直接 Err
pub fn write_rating(path: &Path, name: &str, r: u8) -> Result<(), String> {
    let r = r.min(5);
    match rating_kind(name).ok_or_else(|| format!("格式 {name} 不支持评级写入"))? {
        RatingKind::Sidecar => write_sidecar_rating(path, r),
        RatingKind::Embedded => embedded_set_rating(path, r),
    }
}

/// 从文件头部字节解析嵌入式 XMP 评级。头部已含 JPEG APP1 / PNG iTXt /
/// WebP XMP 块（均在文件头 64KB 内），直接文本搜索；非目标格式签名
/// 返回 None（防御：扫描只对矩阵名单调用，此处按容器签名二次过滤）
fn read_embedded_xmp(data: &[u8]) -> Option<u8> {
    let ok = data.starts_with(&[0xff, 0xd8, 0xff]) // JPEG SOI + marker
        || data.starts_with(&[0x89, b'P', b'N', b'G']) // PNG 签名
        || (data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP");
    if !ok {
        return None;
    }
    parse_xmp_rating(&String::from_utf8_lossy(data))
}

/// 读文件头部（≤512KB；文件更小则到 EOF）。XMP 在 JPEG APP1 / PNG iTXt /
/// WebP XMP 块均位于文件头部——单次读覆盖
fn read_head_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 512 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
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

// ---------------------------------------------------------------------------
// XMP 文本手术（无 XML 解析器，按标记文本级编辑；其余内容原样保留）
// ---------------------------------------------------------------------------

/// 原位替换 Rating 值。处理元素式（`<xmp:Rating>3</xmp:Rating>` 成对
/// 扫描）与属性式（`xmp:Rating="3"`）两种形态；未找到任何可替换
/// 标记 = None（调用方走 xmp_insert_rating）
fn xmp_replace_rating(text: &str, r: u8) -> Option<String> {
    const OPEN: &str = "<xmp:Rating>";
    const CLOSE: &str = "</xmp:Rating>";
    let mut changed = false;

    // 第一遍：元素式成对游标扫描（闭标签内含标记，成对消费不会漏进属性遍历）
    let mut mid = String::with_capacity(text.len() + 8);
    let mut rest = text;
    while let Some(p) = rest.find(OPEN) {
        let val_start = p + OPEN.len();
        match rest[val_start..].find(CLOSE) {
            Some(rel) => {
                mid.push_str(&rest[..p]);
                mid.push_str(OPEN);
                mid.push_str(&r.to_string());
                mid.push_str(CLOSE);
                rest = &rest[val_start + rel + CLOSE.len()..];
                changed = true;
            }
            None => {
                // 有开无闭（畸形）：原样保留后缀，跳出防死循环
                mid.push_str(&rest[p..]);
                rest = "";
                break;
            }
        }
    }
    mid.push_str(rest);

    // 第二遍：属性式逐处替换
    let mut out = String::with_capacity(mid.len());
    let mut tail = mid.as_str();
    while let Some((before, after)) = tail.split_once(XMP_RATING_MARK) {
        let a = after.trim_start();
        if let Some(v) = a.strip_prefix('=') {
            let v = v.trim_start();
            let mut hit = false;
            for q in ['"', '\''] {
                if let Some(inner) = v.strip_prefix(q) {
                    if let Some(end) = inner.find(q) {
                        // mark + 空白 + '=' + 空白 + 引号 + 新值 + 闭引号起的余部
                        out.push_str(before);
                        out.push_str(XMP_RATING_MARK);
                        out.push_str(&after[..after.len() - a.len()]);
                        out.push('=');
                        out.push_str(&v[..v.len() - inner.len()]);
                        out.push_str(&r.to_string());
                        out.push_str(&inner[end..]);
                        tail = &inner[end + 1..];
                        changed = true;
                        hit = true;
                        break;
                    }
                }
            }
            if hit {
                continue;
            }
        }
        // 非属性处（闭标签/声明）：原样保留标记继续
        out.push_str(before);
        out.push_str(XMP_RATING_MARK);
        tail = after;
    }
    out.push_str(tail);
    changed.then_some(out)
}

/// 无 Rating 标记时追加独立 rdf:Description（`</rdf:RDF>` 前插入；
/// 不做属性合并——无 XML 解析器，独立节点对 RDF 语义合法且
/// Lightroom/digiKam 兼容）。找不到 `</rdf:RDF>` = None（XMP 结构不识别）
fn xmp_insert_rating(text: &str, r: u8) -> Option<String> {
    let idx = text.find("</rdf:RDF>")?;
    let desc = format!(
        "<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\n\
         <xmp:Rating>{r}</xmp:Rating>\n\
         </rdf:Description>\n"
    );
    let mut out = String::with_capacity(text.len() + desc.len());
    out.push_str(&text[..idx]);
    out.push_str(&desc);
    out.push_str(&text[idx..]);
    Some(out)
}

/// 剥离 Rating（元素式成对删除 + 属性式删除），其余内容原样保留
///（清除语义：包内 dc:title 等其它字段不受影响）
fn xmp_strip_rating(text: &str) -> String {
    const OPEN: &str = "<xmp:Rating>";
    const CLOSE: &str = "</xmp:Rating>";

    let mut mid = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find(OPEN) {
        let val_start = p + OPEN.len();
        match rest[val_start..].find(CLOSE) {
            Some(rel) => {
                mid.push_str(&rest[..p]);
                rest = &rest[val_start + rel + CLOSE.len()..];
            }
            None => {
                mid.push_str(&rest[p..]);
                rest = "";
                break;
            }
        }
    }
    mid.push_str(rest);

    let mut out = String::with_capacity(mid.len());
    let mut tail = mid.as_str();
    while let Some((before, after)) = tail.split_once(XMP_RATING_MARK) {
        let a = after.trim_start();
        if let Some(v) = a.strip_prefix('=') {
            let v = v.trim_start();
            let mut hit = false;
            for q in ['"', '\''] {
                if let Some(inner) = v.strip_prefix(q) {
                    if let Some(end) = inner.find(q) {
                        out.push_str(before); // 整段属性（mark 到闭引号）删除
                        tail = &inner[end + 1..];
                        hit = true;
                        break;
                    }
                }
            }
            if hit {
                continue;
            }
        }
        out.push_str(before);
        out.push_str(XMP_RATING_MARK);
        tail = after;
    }
    out.push_str(tail);
    out
}

/// 嵌入式写入的文本决策：existing = 容器内已有 XMP 包文本。
/// r>0：有包→原位替换，无标记→追加 Description，两者皆不可→Err；
/// r=0：有包→剥离后写回，无包→Ok 不动文件
fn xmp_new_text(existing: Option<&str>, r: u8) -> Result<Option<String>, String> {
    match (existing, r) {
        (None, 0) => Ok(None),
        (None, _) => Ok(Some(xmp_packet(r))),
        (Some(txt), 0) => Ok(Some(xmp_strip_rating(txt))),
        (Some(txt), _) => xmp_replace_rating(txt, r)
            .or_else(|| xmp_insert_rating(txt, r))
            .map(Some)
            .ok_or_else(|| "XMP 包结构不识别（无 Rating 标记且缺少 </rdf:RDF>）".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 嵌入式写链（img-parts 容器手术，逐格式分流）
// ---------------------------------------------------------------------------

/// JPEG APP1 XMP 主包前缀（XMP 规范 Part 3）；Extended XMP 段前缀
/// 为 "http://ns.adobe.com/xmp/extension/"，前缀比对天然不误伤
const JPEG_XMP_PREFIX: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// PNG iTXt XMP 关键字（XMP 规范 Part 3：iTXt + 关键字
/// XML:com.adobe.com.xmp，未压缩）
const PNG_XMP_KEYWORD: &[u8] = b"XML:com.adobe.com.xmp\0";

/// 嵌入式评级写入按扩展名分流
fn embedded_set_rating(path: &Path, r: u8) -> Result<(), String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "jpe" => jpeg_set_xmp(path, r),
        "png" => png_set_xmp(path, r),
        "webp" => webp_set_xmp(path, r),
        _ => Err(format!("格式 {ext} 不支持嵌入式评级")),
    }
}

/// JPEG：定位旧主包（APP1 + XMP 前缀）→ 文本编辑 → 重建 APP1 段
///（插入 SOI 之后）。Extended XMP 段前缀不同，retain 天然不动
fn jpeg_set_xmp(path: &Path, r: u8) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {e}"))?;
    let mut jpeg = Jpeg::from_bytes(data.into()).map_err(|e| format!("JPEG 解析失败: {e}"))?;
    let is_xmp = |seg: &JpegSegment| {
        seg.marker() == markers::APP1 && seg.contents().starts_with(JPEG_XMP_PREFIX)
    };
    let existing = jpeg.segments().iter().find_map(|seg| {
        if is_xmp(seg) {
            Some(String::from_utf8_lossy(&seg.contents()[JPEG_XMP_PREFIX.len()..]).into_owned())
        } else {
            None
        }
    });
    let new_text = xmp_new_text(existing.as_deref(), r)?;
    jpeg.segments_mut().retain(|seg| !is_xmp(seg));
    if let Some(text) = new_text {
        let mut contents = Vec::with_capacity(JPEG_XMP_PREFIX.len() + text.len());
        contents.extend_from_slice(JPEG_XMP_PREFIX);
        contents.extend_from_slice(text.as_bytes());
        if contents.len() > 65533 {
            return Err("XMP 包超过 JPEG APP1 容量上限（64KB）".to_string());
        }
        // APP1 段插在 SOI（index 0）之后
        jpeg.segments_mut()
            .insert(1, JpegSegment::new_with_contents(markers::APP1, contents.into()));
    }
    let mut out = Vec::new();
    jpeg.encoder()
        .write_to(&mut out)
        .map_err(|e| format!("JPEG 编码失败: {e}"))?;
    std::fs::write(path, out).map_err(|e| format!("写入文件失败: {e}"))
}

/// PNG：定位旧 XMP iTXt（关键字 + flag/method/lang/translated 四个
/// 0x00）→ 文本编辑 → 重建 iTT 块（插 IHDR 之后）。压缩形态
///（flag≠0）不识别 → 视为无包走最小模板重建
fn png_set_xmp(path: &Path, r: u8) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {e}"))?;
    let mut png = Png::from_bytes(data.into()).map_err(|e| format!("PNG 解析失败: {e}"))?;
    let is_xmp = |c: &PngChunk| c.kind() == *b"iTXt" && c.contents().starts_with(PNG_XMP_KEYWORD);
    let existing = png.chunks().iter().find_map(|c| {
        if is_xmp(c) {
            // keyword\0 后依次为 compression flag / method / lang\0 / translated\0
            let rest = &c.contents()[PNG_XMP_KEYWORD.len()..];
            if rest.len() >= 4 && rest[..4] == [0, 0, 0, 0] {
                Some(String::from_utf8_lossy(&rest[4..]).into_owned())
            } else {
                None
            }
        } else {
            None
        }
    });
    let new_text = xmp_new_text(existing.as_deref(), r)?;
    png.chunks_mut().retain(|c| !is_xmp(c));
    if let Some(text) = new_text {
        let mut contents = Vec::with_capacity(PNG_XMP_KEYWORD.len() + 4 + text.len());
        contents.extend_from_slice(PNG_XMP_KEYWORD);
        contents.extend_from_slice(&[0, 0, 0, 0]);
        contents.extend_from_slice(text.as_bytes());
        let chunk = PngChunk::new(*b"iTXt", contents.into());
        png.chunks_mut().insert(1, chunk); // IHDR（index 0）之后
    }
    let mut out = Vec::new();
    png.encoder()
        .write_to(&mut out)
        .map_err(|e| format!("PNG 编码失败: {e}"))?;
    std::fs::write(path, out).map_err(|e| format!("写入文件失败: {e}"))
}

/// WebP：仅扩展格式（VP8X 块）有元数据容器；简单 VP8/VP8L 显式 Err
///（评级留 dirty，不静默兜底）。XMP 块追加在尾部（规范位置：图像
/// 数据之后）
fn webp_set_xmp(path: &Path, r: u8) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {e}"))?;
    let mut webp = WebP::from_bytes(data.into()).map_err(|e| format!("WebP 解析失败: {e}"))?;
    if webp.kind() != VP8Kind::VP8X {
        return Err("WebP 简单格式（无 VP8X 块）不支持嵌入评级".to_string());
    }
    let existing = webp
        .chunk_by_id(CHUNK_XMP)
        .and_then(|c| c.content().data())
        .map(|d| String::from_utf8_lossy(d).into_owned());
    let new_text = xmp_new_text(existing.as_deref(), r)?;
    webp.remove_chunks_by_id(CHUNK_XMP);
    if let Some(text) = new_text {
        let chunk = RiffChunk::new(CHUNK_XMP, RiffContent::Data(text.into_bytes().into()));
        webp.chunks_mut().push(chunk);
    }
    let mut out = Vec::new();
    webp.encoder()
        .write_to(&mut out)
        .map_err(|e| format!("WebP 编码失败: {e}"))?;
    std::fs::write(path, out).map_err(|e| format!("写入文件失败: {e}"))
}

// ---------------------------------------------------------------------------
// 方向链
// ---------------------------------------------------------------------------

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
    //（重开 app 方向即丢失，实测踩中）。改为 img-parts 注入标准 eXIf 块
    //（set_exif 自动插在 IHDR 之后、自动算 CRC），tag 内容仍由
    // little_exif 编码（encode() 输出 = 标准 TIFF blob）
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
/// → img-parts `Png::set_exif` 替换 eXIf 块（旧块全丢弃，新块插在
/// IHDR 之后——PNG ext 1.5 要求 eXIf 位于 IHDR 后、首个 IDAT 前）。
/// 其余 EXIF tag 经 Metadata 全量保留
fn write_png_orientation(path: &Path, o: u8) -> Result<(), String> {
    let mut md = match little_exif::metadata::Metadata::new_from_path(path) {
        Ok(md) => md,
        Err(_) => little_exif::metadata::Metadata::new(),
    };
    md.set_tag(little_exif::exif_tag::ExifTag::Orientation(vec![o as u16]));
    let tiff = md.encode().map_err(|e| format!("编码 EXIF 失败: {e}"))?;
    let data = std::fs::read(path).map_err(|e| format!("读取 PNG 失败: {e}"))?;
    let mut png = Png::from_bytes(data.into()).map_err(|e| format!("PNG 解析失败: {e}"))?;
    png.set_exif(Some(tiff.into()));
    let mut out = Vec::new();
    png.encoder()
        .write_to(&mut out)
        .map_err(|e| format!("PNG 编码失败: {e}"))?;
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

    /// 矩阵名单：嵌入式（jpg/jpeg/jpe/png/webp）/ sidecar
    ///（tif/tiff/相机RAW/bmp/gif）/ 名单外 None
    #[test]
    fn rating_kind_矩阵() {
        use RatingKind::{Embedded, Sidecar};
        for n in ["a.jpg", "a.jpeg", "a.jpe", "a.PNG", "a.WebP"] {
            assert_eq!(rating_kind(n), Some(Embedded), "{n}");
        }
        for n in [
            "a.tif", "a.tiff", "a.arw", "a.nef", "a.nrw", "a.srf", "a.sr2", "a.arq", "a.bmp", "a.gif",
        ] {
            assert_eq!(rating_kind(n), Some(Sidecar), "{n}");
        }
        for n in ["a.txt", "a.mp4", "a.nv12", "noext", "a.zip"] {
            assert_eq!(rating_kind(n), None, "{n}");
        }
    }

    /// read_rating 矩阵语义：嵌入式伪 jpg 文本命中；bmp 含 Rating 文本
    /// 但无 sidecar → None（**钉死无跨格式兜底**）；写 sidecar 后命中
    #[test]
    fn read_rating_矩阵无兜底() {
        let dir =
            std::env::temp_dir().join(format!("archview_meta_test_rrm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 嵌入式：JPEG 签名 + 属性式 Rating
        let emb = dir.join("emb.jpg");
        std::fs::write(&emb, b"\xff\xd8\xff\xe0<rdf:Description xmp:Rating=\"5\"/>").unwrap();
        assert_eq!(read_rating(&emb, "emb.jpg"), Some(5));

        // sidecar：bmp 文件内含 Rating 文本 ≠ sidecar 评级（无兜底）
        let bmp = dir.join("p.bmp");
        std::fs::write(&bmp, b"BMxxxx<rdf:Description xmp:Rating=\"5\"/>").unwrap();
        assert_eq!(read_rating(&bmp, "p.bmp"), None);
        write_sidecar_rating(&bmp, 2).unwrap();
        assert_eq!(read_rating(&bmp, "p.bmp"), Some(2));
        write_sidecar_rating(&bmp, 0).unwrap();

        // 名单外格式 → None
        assert_eq!(read_rating(&emb, "emb.txt"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 文本手术三件套：原位替换（元素/属性/多值共存）、追加、剥离
    ///（其余内容保留）
    #[test]
    fn xmp_文本手术三件套() {
        // 原位替换：元素式
        let t = xmp_packet(1);
        let t = xmp_replace_rating(&t, 4).unwrap();
        assert_eq!(parse_xmp_rating(&t), Some(4));
        assert!(t.contains("<xmp:Rating>4</xmp:Rating>"));
        // 原位替换：属性式（含空白变体）
        let a = "<rdf:Description xmp:Rating='2' dc:title='T'/>";
        let a = xmp_replace_rating(a, 5).unwrap();
        assert_eq!(parse_xmp_rating(&a), Some(5));
        assert!(a.contains("xmp:Rating='5'"));
        assert!(a.contains("dc:title='T'"), "其余属性不得动");
        // 属性式与元素式共存：两处都替换
        let both = "<xmp:Rating>1</xmp:Rating><x xmp:Rating=\"2\"/>";
        let both = xmp_replace_rating(both, 3).unwrap();
        assert!(!both.contains(">1<") && !both.contains("\"2\""));
        // 无标记 → None
        assert!(xmp_replace_rating("<rdf:RDF></rdf:RDF>", 3).is_none());
        // 有开无闭畸形：不 panic、原样返回且 None
        assert!(xmp_replace_rating("<xmp:Rating>1", 3).is_none());

        // 追加：无 Rating 的包（LR 写过 title）→ 独立 Description
        let lr = "<?xpacket begin=''?><x:xmpmeta><rdf:RDF>\
                  <rdf:Description rdf:about='' dc:title='Hello'/>\
                  </rdf:RDF></x:xmpmeta><?xpacket end='w'?>";
        let ins = xmp_insert_rating(lr, 3).unwrap();
        assert_eq!(parse_xmp_rating(&ins), Some(3));
        assert!(ins.contains("dc:title='Hello'"), "原 Description 不动");
        assert!(ins.matches("<rdf:Description").count() == 2, "独立节点追加");
        // 无 </rdf:RDF> → None
        assert!(xmp_insert_rating("<x:xmpmeta/>", 3).is_none());

        // 剥离：元素式 + 属性式都删，其余内容保留
        let both = xmp_packet(2).replace("</rdf:Description>", "");
        let both = format!("{both}<x xmp:Rating=\"1\"/></rdf:Description>");
        let stripped = xmp_strip_rating(&both);
        assert_eq!(parse_xmp_rating(&stripped), None);
        assert!(stripped.contains("xmlns:xmp="), "容器结构保留");
        // 纯属性式剥离
        let s = xmp_strip_rating("<d xmp:Rating='4' keep='k'/>");
        assert_eq!(parse_xmp_rating(&s), None);
        assert!(s.contains("keep='k'"));
    }

    /// 嵌入写链全矩阵往返（真实图片文件）：write→read→clear→幂等 +
    /// 解码完整性。jpg/png 由 image crate 产出；webp 手拼 VP8X 扩展
    /// 格式（image crate 只产简单 VP8，简单格式属显式失败路径另行测试）
    #[test]
    fn 嵌入式评级写读清除往返() {
        let dir =
            std::env::temp_dir().join(format!("archview_meta_test_erw_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // jpg / png
        for (ext, fmt) in [("jpg", image::ImageFormat::Jpeg), ("png", image::ImageFormat::Png)] {
            let p = dir.join(format!("t.{ext}"));
            image::DynamicImage::new_rgb8(4, 4)
                .write_to(&mut std::fs::File::create(&p).unwrap(), fmt)
                .unwrap();
            write_rating(&p, &format!("t.{ext}"), 5).unwrap();
            assert_eq!(read_rating(&p, &format!("t.{ext}")), Some(5), "{ext}");
            write_rating(&p, &format!("t.{ext}"), 2).unwrap();
            assert_eq!(read_rating(&p, &format!("t.{ext}")), Some(2), "{ext} 改评");
            write_rating(&p, &format!("t.{ext}"), 0).unwrap();
            assert_eq!(read_rating(&p, &format!("t.{ext}")), None, "{ext} 清除");
            // 清除后再评（重建包）
            write_rating(&p, &format!("t.{ext}"), 4).unwrap();
            assert_eq!(read_rating(&p, &format!("t.{ext}")), Some(4), "{ext} 重建");
            image::open(&p).unwrap_or_else(|e| panic!("{ext} 写回后解码失败: {e}"));
        }

        // webp：手拼 VP8X 扩展格式（简单 VP8 数据块 + VP8X 头块）
        let wp = dir.join("t.webp");
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut std::fs::File::create(&wp).unwrap(), image::ImageFormat::WebP)
            .unwrap();
        let simple = std::fs::read(&wp).unwrap();
        let parsed = WebP::from_bytes(simple.into()).unwrap();
        // image crate 的 WebP 编码为 lossless（VP8L 块），VP8/VP8L 二选一
        let bits = parsed
            .chunks()
            .iter()
            .find(|c| {
                c.id() == img_parts::webp::CHUNK_VP8 || c.id() == img_parts::webp::CHUNK_VP8L
            })
            .expect("image crate 产出简单 VP8/VP8L")
            .clone();
        let (w, h) = parsed.dimensions().unwrap();
        let mut vp8x = vec![0u8]; // flags
        vp8x.extend_from_slice(&[0, 0, 0]); // reserved（规范要求 VP8X 数据块共 10 字节）
        vp8x.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        vp8x.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        let riff = RiffChunk::new(
            *b"RIFF", // 顶层 chunk id 是 RIFF，WEBP 是 List kind
            RiffContent::List {
                kind: Some(*b"WEBP"),
                subchunks: vec![
                    RiffChunk::new(img_parts::webp::CHUNK_VP8X, RiffContent::Data(vp8x.into())),
                    bits,
                ],
            },
        );
        let mut out = Vec::new();
        WebP::new(riff).unwrap().encoder().write_to(&mut out).unwrap();
        std::fs::write(&wp, &out).unwrap();
        image::open(&wp).expect("手拼 VP8X 可解码");

        write_rating(&wp, "t.webp", 3).unwrap();
        assert_eq!(read_rating(&wp, "t.webp"), Some(3));
        write_rating(&wp, "t.webp", 0).unwrap();
        assert_eq!(read_rating(&wp, "t.webp"), None, "webp 清除");
        write_rating(&wp, "t.webp", 1).unwrap();
        assert_eq!(read_rating(&wp, "t.webp"), Some(1), "webp 清除后再评");
        image::open(&wp).unwrap_or_else(|e| panic!("webp 写回后解码失败: {e}"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 已有 XMP 包内容保留：预置含 dc:title 的包（模拟 LR 写过）→
    /// write_rating 原位替换 → title 仍在 + Rating 命中 → 清除后
    /// title 仍在 + Rating 无
    #[test]
    fn 嵌入式保留已有包内容() {
        let dir =
            std::env::temp_dir().join(format!("archview_meta_test_keep_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("lr.jpg");
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut std::fs::File::create(&p).unwrap(), image::ImageFormat::Jpeg)
            .unwrap();
        // 预置 LR 风格包：title、无 Rating
        let packet = "<?xpacket begin='' id=''?><x:xmpmeta xmlns:x='adobe:ns:meta/'>\
                      <rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>\
                      <rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'>\
                      <dc:title><rdf:Alt><rdf:li xml:lang='x-default'>我的标题</rdf:li></rdf:Alt></dc:title>\
                      </rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end='w'?>";
        let data = std::fs::read(&p).unwrap();
        let mut jpeg = Jpeg::from_bytes(data.into()).unwrap();
        let mut contents = JPEG_XMP_PREFIX.to_vec();
        contents.extend_from_slice(packet.as_bytes());
        jpeg.segments_mut()
            .insert(1, JpegSegment::new_with_contents(markers::APP1, contents.into()));
        let mut out = Vec::new();
        jpeg.encoder().write_to(&mut out).unwrap();
        std::fs::write(&p, &out).unwrap();
        image::open(&p).unwrap();

        write_rating(&p, "lr.jpg", 3).unwrap();
        let text = embedded_packet_text(&p);
        assert_eq!(parse_xmp_rating(&text), Some(3));
        assert!(text.contains("我的标题"), "title 必须保留");

        write_rating(&p, "lr.jpg", 0).unwrap();
        let text = embedded_packet_text(&p);
        assert_eq!(parse_xmp_rating(&text), None);
        assert!(text.contains("我的标题"), "清除后 title 仍保留");

        image::open(&p).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 提取文件内嵌入式 XMP 包文本（JPEG APP1 主包），诊断断言用
    fn embedded_packet_text(path: &Path) -> String {
        let data = std::fs::read(path).unwrap();
        let jpeg = Jpeg::from_bytes(data.into()).unwrap();
        jpeg.segments()
            .iter()
            .find(|s| s.marker() == markers::APP1 && s.contents().starts_with(JPEG_XMP_PREFIX))
            .map(|s| String::from_utf8_lossy(&s.contents()[JPEG_XMP_PREFIX.len()..]).into_owned())
            .expect("XMP 包存在")
    }

    /// WebP 简单格式（image crate 产出，无 VP8X）显式失败：Err + 文件
    /// 字节不变（不静默兜底、不部分写入）
    #[test]
    fn webp_简单格式明确失败() {
        let dir =
            std::env::temp_dir().join(format!("archview_meta_test_wsf_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.webp");
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut std::fs::File::create(&p).unwrap(), image::ImageFormat::WebP)
            .unwrap();
        let before = std::fs::read(&p).unwrap();
        let err = write_rating(&p, "t.webp", 5).unwrap_err();
        assert!(err.contains("简单格式"), "错误文案需指明原因: {err}");
        assert_eq!(std::fs::read(&p).unwrap(), before, "文件字节不得改动");
        // 清除语义对简单格式同样失败（无容器可剥离）
        assert!(write_rating(&p, "t.webp", 0).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 全 meta_writable 格式矩阵诊断：各格式建文件 → write_orientation →
    /// kamadak-exif 读回（与 app 解码链同源）。打印每格式的写/读结果，
    /// 断言全部 Some(6)。含 PNG img-parts eXIf 注入链路
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

    /// 诊断共用：独立临时目录（不得与其他测试共用——其收尾
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
