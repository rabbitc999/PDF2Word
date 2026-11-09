//! 从 OCR 文字框内的像素估算可写入 Word 的字形属性。

use image::RgbImage;

use crate::scan_ocr::PixelRect;

/// OCR 字符的外观估计。字号以原图像素计，由排版阶段换算为磅。
#[derive(Clone)]
pub(crate) struct ScanStyle {
    pub(crate) size_px: f32,
    pub(crate) rgb: [u8; 3],
    pub(crate) bold: bool,
    pub(crate) italic: bool,
}

/// 判断字符是否属于常见的中日韩统一表意文字区间。
pub(crate) fn is_cjk(character: char) -> bool {
    matches!(character, '\u{3400}'..='\u{9fff}' | '\u{20000}'..='\u{323af}')
}

fn luma([red, green, blue]: [u8; 3]) -> u8 {
    ((u32::from(red) * 77 + u32::from(green) * 150 + u32::from(blue) * 29) >> 8) as u8
}

/// 依据前景墨迹的高度、颜色和笔画形状估算字号、字色、粗体及斜体。
pub(crate) fn estimate_style(image: &RgbImage, bounds: PixelRect, text: &str) -> ScanStyle {
    let width = bounds.width() as usize;
    let height = bounds.height() as usize;
    let cjk = text.chars().any(is_cjk);
    let mut luminance = [0u32; 256];
    for y in bounds.top..bounds.bottom {
        for x in bounds.left..bounds.right {
            luminance[luma(image.get_pixel(x, y).0) as usize] += 1;
        }
    }
    let cutoff = (width * height * 3 / 4) as u32;
    let mut seen = 0;
    let background = luminance
        .iter()
        .position(|count| {
            seen += count;
            seen >= cutoff
        })
        .map_or(255, |value| value as u8);
    let threshold = background.saturating_sub(38).min(224);

    // 聚类颜色以忽略抗锯齿边缘，并保留墨迹位置以分析笔画。
    let mut colors = [(0u32, [0u64; 3]); 512];
    let mut ink = Vec::new();
    let mut stroke_widths = Vec::new();
    for y in bounds.top..bounds.bottom {
        let mut stroke = 0u32;
        for x in bounds.left..bounds.right {
            let rgb = image.get_pixel(x, y).0;
            if luma(rgb) <= threshold {
                let bucket = (usize::from(rgb[0] >> 5) << 6)
                    | (usize::from(rgb[1] >> 5) << 3)
                    | usize::from(rgb[2] >> 5);
                colors[bucket].0 += 1;
                for (sum, channel) in colors[bucket].1.iter_mut().zip(rgb) {
                    *sum += u64::from(channel);
                }
                ink.push(((x - bounds.left) as f32, (y - bounds.top) as f32));
                stroke += 1;
            } else if stroke > 0 {
                stroke_widths.push(stroke);
                stroke = 0;
            }
        }
        if stroke > 0 {
            stroke_widths.push(stroke);
        }
    }

    let ink_height = match (ink.first(), ink.last()) {
        (Some(first), Some(last)) => last.1 - first.1 + 1.0,
        _ => bounds.height() as f32,
    };
    let rgb = colors
        .iter()
        .max_by_key(|(count, _)| *count)
        .filter(|(count, _)| *count > 0)
        .map_or([0; 3], |(count, sum)| {
            sum.map(|channel| (channel / u64::from(*count)) as u8)
        });
    stroke_widths.retain(|&stroke| stroke as f32 <= ink_height * 0.4);
    stroke_widths.sort_unstable();
    let stroke = stroke_widths
        .get(stroke_widths.len() * 3 / 5)
        .map_or(0.0, |&width| width as f32);

    // 把字形按多个角度反向剪切。竖笔画变直时，纵向投影会比原图更集中。
    let mut italic = false;
    if !cjk && ink_height >= 12.0 && text.bytes().filter(u8::is_ascii_alphabetic).count() >= 4 {
        let mut projection = vec![0u32; width + height];
        let mut concentration = |slope: f32| {
            projection.fill(0);
            for &(x, y) in &ink {
                projection[(x + slope * y).round() as usize] += 1;
            }
            projection
                .iter()
                .map(|&count| f64::from(count).powi(2))
                .sum::<f64>()
        };
        let vertical = concentration(0.0);
        italic = [0.18, 0.25, 0.32]
            .into_iter()
            .any(|slope| concentration(slope) > vertical * 1.075);
    }
    // 短数字和短标签的笔画样本较少，采用更保守的粗体阈值。
    let bold_threshold = if cjk {
        0.125
    } else if text.bytes().filter(u8::is_ascii_alphabetic).count() < 8 {
        0.225
    } else {
        0.158
    };
    ScanStyle {
        size_px: ink_height * if cjk { 1.15 } else { 1.32 },
        rgb,
        bold: stroke / ink_height.max(1.0) >= bold_threshold,
        italic,
    }
}
