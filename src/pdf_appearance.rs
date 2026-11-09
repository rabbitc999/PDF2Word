//! 从 PDF 字体资源、边框路径和图形路径恢复普通 PDF 的外观。

use std::collections::HashMap;

use pdf_oxide::{
    PdfDocument,
    elements::PathContent,
    geometry::Rect,
    layout::{Color, TextSpan},
};
use ttf_parser::{Face, name_id};

use crate::scan_ocr::PixelRect;

/// 字体的 Word 家族名以及字体文件自身记录的字重和斜体属性。
pub(crate) struct FontFace {
    pub(crate) family: String,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
}

/// 去除 PDF 为嵌入字体子集添加的六位标记。
fn base_name(name: &str) -> &str {
    match name.split_once('+') {
        Some((prefix, base))
            if prefix.len() == 6 && prefix.bytes().all(|b| b.is_ascii_uppercase()) =>
        {
            base
        }
        _ => name,
    }
}

/// 读取内嵌 TrueType 和 OpenType 字体的家族名称。其他字体由资源名回退识别。
pub(crate) fn embedded_faces(pdf: &PdfDocument) -> HashMap<String, FontFace> {
    let Ok(fonts) = pdf.extract_embedded_fonts() else {
        return HashMap::new();
    };
    fonts
        .into_iter()
        .filter_map(|(name, bytes)| {
            let face = Face::parse(&bytes, 0).ok()?;
            let family = [name_id::TYPOGRAPHIC_FAMILY, name_id::FAMILY]
                .into_iter()
                .find_map(|id| {
                    face.names()
                        .into_iter()
                        .filter(|name| name.name_id == id)
                        .find_map(|name| {
                            name.to_string().filter(|family| !family.trim().is_empty())
                        })
                })?;
            Some((
                base_name(&name).to_owned(),
                FontFace {
                    family,
                    bold: face.is_bold(),
                    italic: face.is_italic(),
                },
            ))
        })
        .collect()
}

/// 将字体资源名和常见 PostScript 别名转为 Word 使用的字体家族。
pub(crate) fn resolve_font(
    name: &str,
    resources: &HashMap<String, String>,
    embedded: &HashMap<String, FontFace>,
) -> FontFace {
    let face = base_name(resources.get(name).map_or(name, String::as_str));
    let compact = face.to_ascii_lowercase().replace([' ', '-', ','], "");
    let name_bold = compact.contains("bold") || compact.ends_with("demi");
    let name_italic = compact.contains("italic") || compact.contains("oblique");
    if let Some(font) = embedded.get(face) {
        return FontFace {
            family: font.family.clone(),
            bold: font.bold || name_bold,
            italic: font.italic || name_italic,
        };
    }
    let family = if compact.starts_with("arialnarrow") {
        "Arial Narrow"
    } else if compact.starts_with("arialblack") {
        "Arial Black"
    } else if compact.starts_with("arial") || compact.starts_with("helvetica") {
        "Arial"
    } else if compact.starts_with("timesnewroman")
        || compact.starts_with("texgyretermes")
        || face.starts_with("Times-")
        || face == "Times"
    {
        "Times New Roman"
    } else if compact.starts_with("cmutypewriter") {
        "Consolas"
    } else if compact.starts_with("courier") {
        "Courier New"
    } else if compact.starts_with("calibri") {
        "Calibri"
    } else if compact.starts_with("simsun") || face == "宋体" {
        "SimSun"
    } else if compact.starts_with("simhei") || face == "黑体" {
        "SimHei"
    } else if compact.starts_with("microsoftyahei") || face == "微软雅黑" {
        "Microsoft YaHei"
    } else {
        match face.rsplit_once(['-', ',']) {
            Some((family, variant))
                if [
                    "regular", "roman", "bold", "italic", "oblique", "medium", "light", "black",
                ]
                .iter()
                .any(|suffix| variant.to_ascii_lowercase().starts_with(suffix)) =>
            {
                family
            }
            _ => face,
        }
    };
    FontFace {
        family: family.to_owned(),
        bold: name_bold,
        italic: name_italic,
    }
}

/// 将 PDF 的浮点 RGB 分量转为 Word 颜色值。
pub(crate) fn rgb(color: Color) -> [u8; 3] {
    [color.r, color.g, color.b].map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn grow(bounds: Rect, amount: f32) -> Rect {
    Rect::new(
        bounds.x - amount,
        bounds.y - amount,
        bounds.width + 2.0 * amount,
        bounds.height + 2.0 * amount,
    )
}

/// 用靠近表格文字范围的长边线估算网格边界。
pub(crate) fn table_columns(paths: &[PathContent], bounds: Rect, columns: usize) -> Vec<f32> {
    let search = grow(bounds, 32.0);
    let mut edges: Vec<_> = paths
        .iter()
        .filter_map(|path| {
            let line = path.rendered_bbox();
            (path.stroke_color.is_some()
                && line.width <= 3.0
                && line.height >= bounds.height * 0.65
                && search.intersects(&line))
            .then_some(line.x + line.width * 0.5)
        })
        .collect();
    edges.sort_by(f32::total_cmp);
    edges.dedup_by(|a, b| (*a - *b).abs() <= 2.0);
    if edges.len() == columns + 1 {
        edges
    } else {
        Vec::new()
    }
}

/// 从网格路径中选出代表性的边框颜色和线宽。
pub(crate) fn table_border(paths: &[PathContent], bounds: Rect) -> Option<([u8; 3], f32)> {
    let search = grow(bounds, 14.0);
    paths.iter().find_map(|path| {
        let color = path.stroke_color?;
        let line = path.rendered_bbox();
        let horizontal = line.height <= 3.0 && line.width >= bounds.width * 0.7;
        let vertical = line.width <= 3.0 && line.height >= bounds.height * 0.65;
        ((horizontal || vertical) && search.intersects(&line))
            .then_some((rgb(color), path.stroke_width))
    })
}

/// 根据覆盖单元格中心的填充矩形恢复底色。
pub(crate) fn cell_fill(paths: &[PathContent], bounds: Rect, x: f32, y: f32) -> Option<[u8; 3]> {
    paths.iter().rev().find_map(|path| {
        let color = path.fill_color?;
        let rect = path.bbox;
        (path.is_rectangle()
            && rect.width * rect.height <= bounds.width * bounds.height * 1.8
            && (rect.x..=rect.x + rect.width).contains(&x)
            && (rect.y..=rect.y + rect.height).contains(&y))
        .then_some(rgb(color))
    })
}

/// 聚合正文和表格以外的矢量路径，返回适合渲染裁切的图形区域。
pub(crate) fn vector_regions(
    paths: &[PathContent],
    tables: &[Rect],
    text: &[&TextSpan],
    page: Rect,
) -> Vec<Rect> {
    let mut regions: Vec<Rect> = Vec::new();
    for path in paths {
        let bounds = path.rendered_bbox();
        if (bounds.width < 3.0 && bounds.height < 3.0)
            || bounds.width * bounds.height > page.width * page.height * 0.65
            || tables
                .iter()
                .any(|&table| grow(table, 24.0).contains_rect(&bounds))
            || (path.is_rectangle()
                && path.fill_color.is_some()
                && text.iter().any(|span| bounds.contains_rect(&span.bbox)))
        {
            continue;
        }
        let mut group = bounds;
        while let Some(index) = regions
            .iter()
            .position(|other| grow(*other, 9.0).intersects(&group))
        {
            group = group.union(&regions.swap_remove(index));
        }
        regions.push(group);
    }
    regions
        .into_iter()
        .filter_map(|bounds| {
            if bounds.width < 16.0
                || bounds.height < 12.0
                || bounds.width * bounds.height > page.width * page.height * 0.65
            {
                return None;
            }
            let bounds = grow(bounds, 2.0);
            let x = bounds.x.max(page.x);
            let y = bounds.y.max(page.y);
            let right = (bounds.x + bounds.width).min(page.x + page.width);
            let top = (bounds.y + bounds.height).min(page.y + page.height);
            (x < right && y < top).then_some(Rect::new(x, y, right - x, top - y))
        })
        .collect()
}

/// 投影嵌入位图及矢量图的边界，为浅色插图和稀疏线稿提供裁切位置。
pub(crate) fn picture_pixel_regions(
    pdf: &PdfDocument,
    page: usize,
    media: Rect,
    pixels: (u32, u32),
) -> Vec<PixelRect> {
    let Ok(rotation) = pdf.get_page_rotation(page) else {
        return Vec::new();
    };
    if media.width <= 0.0 || media.height <= 0.0 {
        return Vec::new();
    }
    let project = |x: f32, y: f32| {
        let x = ((x - media.x) / media.width).clamp(0.0, 1.0);
        let y = ((y - media.y) / media.height).clamp(0.0, 1.0);
        let (x, y) = match rotation.rem_euclid(360) {
            90 => (y, x),
            180 => (1.0 - x, y),
            270 => (1.0 - y, 1.0 - x),
            _ => (x, 1.0 - y),
        };
        (x * pixels.0 as f32, y * pixels.1 as f32)
    };
    // 位图位置来自页面内容流，无需解码原图，也不受颜色或对比度影响。
    let bitmaps = pdf.page_image_handles(page).map_or_else(
        |_| Vec::new(),
        |images| {
            images
                .into_iter()
                .map(|image| image.bbox)
                .collect::<Vec<_>>()
        },
    );
    let vectors = pdf.extract_paths(page).map_or_else(
        |_| Vec::new(),
        |paths| vector_regions(&paths, &[], &[], media),
    );
    bitmaps
        .into_iter()
        .chain(vectors)
        .filter_map(|region| {
            let a = project(region.x, region.y);
            let b = project(region.x + region.width, region.y + region.height);
            let rect = PixelRect {
                left: a.0.min(b.0).floor() as u32,
                top: a.1.min(b.1).floor() as u32,
                right: (a.0.max(b.0).ceil() as u32).min(pixels.0),
                bottom: (a.1.max(b.1).ceil() as u32).min(pixels.1),
            };
            if rect.left >= rect.right || rect.top >= rect.bottom {
                return None;
            }
            // 整页扫描底图由 OCR 处理，避免它盖住重建的文字和表格。
            let covers_page = f64::from(rect.width()) >= f64::from(pixels.0) * 0.9
                && f64::from(rect.height()) >= f64::from(pixels.1) * 0.9;
            (!covers_page).then_some(rect)
        })
        .collect()
}
