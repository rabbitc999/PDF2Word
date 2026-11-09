//! 使用内置模型识别扫描页，并按像素位置恢复有边框的表格及插图。

use std::fs;

use crate::scan_style::{ScanStyle, estimate_style, is_cjk};
use crate::word_spacing::repair_spacing;
use anyhow::{Context, Result, bail};
use image::{GrayImage, RgbImage};
use rusto::{DetectTextResult, InitializeConfig, OcrRunOptions, RustO};

/// 扫描图上的矩形，坐标从左上角起算，右边和下边为开区间。
#[derive(Clone, Copy, Debug)]
pub(crate) struct PixelRect {
    pub(crate) left: u32,
    pub(crate) top: u32,
    pub(crate) right: u32,
    pub(crate) bottom: u32,
}

impl PixelRect {
    pub(crate) fn width(self) -> u32 {
        self.right - self.left
    }

    pub(crate) fn height(self) -> u32 {
        self.bottom - self.top
    }

    fn overlaps(self, other: Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }

    fn contains(self, other: Self) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }

    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

/// 单页识别后的元素，边框用于换算为 PDF 页面坐标。
pub(crate) enum ScanElement {
    Text(Vec<ScanLine>),
    Table {
        column_widths: Vec<u32>,
        rows: Vec<Vec<Vec<ScanLine>>>,
        fills: Vec<Vec<[u8; 3]>>,
        border_rgb: [u8; 3],
        border_px: u32,
    },
    Picture,
}

/// 文字内容、页面位置与从墨迹估算的样式。
#[derive(Clone)]
pub(crate) struct ScanLine {
    pub(crate) bounds: PixelRect,
    pub(crate) text: String,
    pub(crate) style: ScanStyle,
}

/// 按需初始化 OCR 引擎，每次转换只加载一次模型。
pub(crate) struct ScanOcr {
    engine: RustO,
    _model_dir: tempfile::TempDir,
}

impl ScanOcr {
    /// 将 EXE 内的模型写入临时目录，并在引擎销毁后自动清理。
    pub(crate) fn new() -> Result<Self> {
        let dir = tempfile::tempdir().context("无法创建 OCR 模型临时目录")?;
        let det = dir.path().join("det.rten");
        let rec = dir.path().join("rec.rten");
        let dict = dir.path().join("dict.txt");
        for (path, contents) in [
            (&det, include_bytes!("../models/det.rten").as_slice()),
            (&rec, include_bytes!("../models/rec.rten").as_slice()),
            (&dict, include_bytes!("../models/dict.txt").as_slice()),
        ] {
            fs::write(path, contents).context("无法展开内置 OCR 模型")?;
        }
        let config = InitializeConfig::ppv6(det, rec, dict).with_text_score(0.45);
        let engine = RustO::initialize(config).context("无法加载内置 OCR 模型")?;
        Ok(Self {
            engine,
            _model_dir: dir,
        })
    }

    /// 识别中英文，先将文字分配到网格单元格，再寻找剩余图像区域。
    pub(crate) fn recognize(
        &mut self,
        image: &RgbImage,
        picture_hints: &[PixelRect],
    ) -> Result<Vec<(PixelRect, ScanElement)>> {
        let (width, height) = image.dimensions();
        let result = self.engine.detect_text_rgb(
            image.as_raw(),
            width,
            height,
            width as usize * 3,
            &OcrRunOptions::default(),
        )?;
        let DetectTextResult::Structured(results) = result else {
            bail!("OCR 引擎没有返回文字位置");
        };
        let mut lines: Vec<_> = results
            .into_iter()
            .filter_map(|line| {
                let frame = line.frame;
                let bounds = PixelRect {
                    left: frame.left.floor().clamp(0.0, width as f32) as u32,
                    top: frame.top.floor().clamp(0.0, height as f32) as u32,
                    right: (frame.left + frame.width).ceil().clamp(0.0, width as f32) as u32,
                    bottom: (frame.top + frame.height).ceil().clamp(0.0, height as f32) as u32,
                };
                if bounds.left < bounds.right
                    && bounds.top < bounds.bottom
                    && !line.text.trim().is_empty()
                {
                    let text = match repair_spacing(image, bounds, &line.text) {
                        Some(corrected) => corrected,
                        None => line.text,
                    };
                    Some(ScanLine {
                        bounds,
                        style: estimate_style(image, bounds, &text),
                        text,
                    })
                } else {
                    None
                }
            })
            .collect();
        if lines.is_empty() {
            return Ok(Vec::new());
        }

        let gray = image::imageops::grayscale(image);
        lines.sort_by_key(|line| (line.bounds.top, line.bounds.left));
        let mut masked = vec![0u8; width as usize * height as usize];
        let mut elements = Vec::new();

        // 只有文字确实落在多个行列的规则网格内时，才将其改写为 Word 表格。
        for grid in find_grids(&gray) {
            let bounds = grid.bounds();
            let mut rows = vec![vec![Vec::new(); grid.x.len() - 1]; grid.y.len() - 1];
            for line in &lines {
                if let Some((row, column)) = grid.cell(line.bounds) {
                    rows[row][column].push(line.clone());
                }
            }
            let filled_rows = rows
                .iter()
                .filter(|row| row.iter().any(|cell| !cell.is_empty()))
                .count();
            let filled_columns = (0..grid.x.len() - 1)
                .filter(|&column| rows.iter().any(|row| !row[column].is_empty()))
                .count();
            if filled_rows >= 2 && filled_columns >= 2 {
                lines.retain(|line| grid.cell(line.bounds).is_none());
                mask_region(&mut masked, width, height, bounds, 3);
                let column_widths = grid.x.windows(2).map(|pair| pair[1] - pair[0]).collect();
                let (fills, border_rgb) = grid.colors(image);
                elements.push((
                    bounds,
                    ScanElement::Table {
                        column_widths,
                        rows,
                        fills,
                        border_rgb,
                        border_px: grid.stroke_px,
                    },
                ));
            }
        }

        // 遮蔽已识别的字块和表格，再从剩余的连续区域中裁出照片、插图等内容。
        let text_padding = (width.min(height) / 250).clamp(3, 10);
        for line in &lines {
            mask_region(&mut masked, width, height, line.bounds, text_padding);
        }
        let mut pictures = picture_regions(&gray, &masked, 213);
        let page_area = u64::from(width) * u64::from(height);
        let pale_threshold = paper_level(&gray).saturating_sub(5).min(250);
        if pale_threshold > 213 {
            // 对没有嵌入位置的扫描图使用相对纸色的阈值。大面积底色或含正文的阴影不作为插图。
            for region in picture_regions(&gray, &masked, pale_threshold) {
                let area = u64::from(region.width()) * u64::from(region.height());
                if area * 5 < page_area * 3
                    && !lines.iter().any(|line| region.contains(line.bounds))
                {
                    merge_picture(&mut pictures, region);
                }
            }
        }
        for &region in picture_hints {
            // 已重建的表格不重复插图。含有正文的图形区域仍由像素分区判断。
            if !elements.iter().any(|(bounds, _)| bounds.overlaps(region))
                && !lines.iter().any(|line| region.contains(line.bounds))
            {
                merge_picture(&mut pictures, region);
            }
        }
        lines.retain(|line| {
            !pictures.iter().any(|picture| {
                // 大范围的底纹或扫描阴影不应遮掉页面上的全部文字。
                let area = u64::from(picture.width()) * u64::from(picture.height());
                area * 5 < page_area * 3 && picture.contains(line.bounds)
            })
        });
        if elements.is_empty() && lines.is_empty() {
            return Ok(Vec::new());
        }
        elements.extend(
            pictures
                .into_iter()
                .map(|bounds| (bounds, ScanElement::Picture)),
        );
        elements.extend(
            lines
                .into_iter()
                .map(|line| (line.bounds, ScanElement::Text(vec![line]))),
        );
        elements.sort_by_key(|(bounds, _)| (bounds.top, bounds.left));
        Ok(group_paragraphs(elements, width))
    }
}

/// 合并重叠的图片区域，避免同一个图框和其中的子形状重复插入。
fn merge_picture(pictures: &mut Vec<PixelRect>, mut bounds: PixelRect) {
    while let Some(index) = pictures.iter().position(|picture| picture.overlaps(bounds)) {
        bounds = bounds.union(pictures.swap_remove(index));
    }
    pictures.push(bounds);
}

/// 合并紧邻且字号、左边距相近的正文行，保留首行缩进所需的位置。
fn group_paragraphs(
    elements: Vec<(PixelRect, ScanElement)>,
    width: u32,
) -> Vec<(PixelRect, ScanElement)> {
    let mut groups: Vec<(PixelRect, ScanElement)> = Vec::new();
    for (bounds, element) in elements {
        if let ScanElement::Text(next) = &element
            && let Some((previous_bounds, ScanElement::Text(previous))) = groups.last_mut()
            && let (Some(last), Some(first)) = (previous.last(), next.first())
        {
            let size = last.style.size_px.max(1.0);
            let same_size = (0.82..=1.22).contains(&(first.style.size_px / size));
            let close = bounds.top >= last.bounds.top
                && bounds.top.saturating_sub(last.bounds.bottom) as f32 <= size * 0.82
                && bounds.top as f32 >= last.bounds.top as f32 + size * 0.42;
            let aligned = bounds.left.abs_diff(last.bounds.left) as f32 <= size * 1.1;
            let long = last.bounds.width() as f32 >= width as f32 * 0.26
                && last.text.chars().count() >= 12;
            // 完整句子后的跨语种文字通常是独立说明，例如中英文并列的图注。
            let translated = last
                .text
                .trim_end()
                .ends_with(['.', '!', '?', '。', '！', '？'])
                && (last.text.chars().any(is_cjk) != first.text.chars().any(is_cjk));
            if same_size && close && aligned && long && !translated {
                *previous_bounds = previous_bounds.union(bounds);
                previous.extend(next.iter().cloned());
                continue;
            }
        }
        groups.push((bounds, element));
    }
    groups
}

struct Rule {
    first: u32,
    last: u32,
    start: u32,
    end: u32,
}

impl Rule {
    fn position(&self) -> u32 {
        (self.first + self.last) / 2
    }

    fn crosses(&self, other: &Self, tolerance: u32) -> bool {
        let x = other.position();
        let y = self.position();
        x >= self.start.saturating_sub(tolerance)
            && x <= self.end + tolerance
            && y >= other.start.saturating_sub(tolerance)
            && y <= other.end + tolerance
    }
}

/// 沿单个方向查找长墨线，忽略文字大小的短线和过厚的深色区域。
fn find_rules(image: &GrayImage, horizontal: bool) -> Vec<Rule> {
    let (width, height) = image.dimensions();
    let (major, minor) = if horizontal {
        (height, width)
    } else {
        (width, height)
    };
    let min_length = (minor / 16).max(32);
    let tolerance = (minor / 450).clamp(2, 7);
    let mut rules: Vec<Rule> = Vec::new();
    for position in 0..major {
        let mut start = None;
        let mut end = 0;
        for index in 0..=minor {
            let dark = index < minor
                && if horizontal {
                    image.get_pixel(index, position)[0] < 165
                } else {
                    image.get_pixel(position, index)[0] < 165
                };
            if dark {
                start.get_or_insert(index);
                end = index + 1;
            } else if let Some(begin) = start
                && (index == minor || index.saturating_sub(end) > tolerance)
            {
                if end - begin >= min_length {
                    let previous = rules
                        .iter_mut()
                        .rev()
                        .take_while(|rule| rule.last + 2 >= position)
                        .find(|rule| {
                            rule.start.abs_diff(begin) <= tolerance
                                && rule.end.abs_diff(end) <= tolerance
                        });
                    if let Some(rule) = previous {
                        rule.last = position;
                    } else {
                        rules.push(Rule {
                            first: position,
                            last: position,
                            start: begin,
                            end,
                        });
                    }
                }
                start = None;
            }
        }
    }
    rules.retain(|rule| rule.last - rule.first <= tolerance * 2);
    rules.sort_by_key(Rule::position);
    rules
}

struct Grid {
    x: Vec<u32>,
    y: Vec<u32>,
    stroke_px: u32,
}

impl Grid {
    fn bounds(&self) -> PixelRect {
        PixelRect {
            left: self.x[0],
            top: self.y[0],
            right: self.x[self.x.len() - 1],
            bottom: self.y[self.y.len() - 1],
        }
    }

    fn cell(&self, bounds: PixelRect) -> Option<(usize, usize)> {
        let center_x = bounds.left + bounds.width() / 2;
        let center_y = bounds.top + bounds.height() / 2;
        let column = self
            .x
            .windows(2)
            .position(|pair| (pair[0]..pair[1]).contains(&center_x))?;
        let row = self
            .y
            .windows(2)
            .position(|pair| (pair[0]..pair[1]).contains(&center_y))?;
        // 横跨多列或多行的识别框保留在正文，避免猜错单元格。
        let overlap_x = bounds.right.min(self.x[column + 1]) - bounds.left.max(self.x[column]);
        let overlap_y = bounds.bottom.min(self.y[row + 1]) - bounds.top.max(self.y[row]);
        (overlap_x * 4 >= bounds.width() * 3 && overlap_y * 4 >= bounds.height() * 3)
            .then_some((row, column))
    }

    /// 从单元格内部和表格外框采样颜色，避免使用文字像素作为底色。
    fn colors(&self, image: &RgbImage) -> (Vec<Vec<[u8; 3]>>, [u8; 3]) {
        let mut fills = Vec::new();
        for y in self.y.windows(2) {
            let mut row = Vec::new();
            for x in self.x.windows(2) {
                let mut samples = Vec::new();
                for dy in [1, 2, 3] {
                    for dx in [1, 2, 3] {
                        samples.push(
                            image
                                .get_pixel(
                                    x[0] + (x[1] - x[0]) * dx / 4,
                                    y[0] + (y[1] - y[0]) * dy / 4,
                                )
                                .0,
                        );
                    }
                }
                samples.sort_by_key(|rgb| rgb.iter().map(|&value| u16::from(value)).sum::<u16>());
                row.push(samples[6]);
            }
            fills.push(row);
        }
        let bounds = self.bounds();
        let border = image
            .get_pixel(bounds.left + bounds.width() / 2, bounds.top)
            .0;
        (fills, border)
    }
}

/// 由贯通的横竖线交点寻找完整网格，避免把图框误当成表格。
fn find_grids(image: &GrayImage) -> Vec<Grid> {
    let horizontal = find_rules(image, true);
    let vertical = find_rules(image, false);
    if horizontal.len() > 256 || vertical.len() > 256 {
        return Vec::new();
    }
    let tolerance = (image.width().min(image.height()) / 240).clamp(3, 12);
    let mut grids: Vec<Grid> = Vec::new();
    for seed in &horizontal {
        let columns: Vec<_> = vertical
            .iter()
            .filter(|rule| seed.crosses(rule, tolerance))
            .collect();
        if columns.len() < 3 || columns.len() > 64 {
            continue;
        }
        let mut x: Vec<_> = columns.iter().map(|rule| rule.position()).collect();
        x.dedup_by(|a, b| a.abs_diff(*b) <= tolerance);
        let mut y: Vec<_> = horizontal
            .iter()
            .filter(|rule| columns.iter().all(|column| rule.crosses(column, tolerance)))
            .map(Rule::position)
            .collect();
        y.dedup_by(|a, b| a.abs_diff(*b) <= tolerance);
        if x.len() >= 3 && y.len() >= 3 {
            let grid = Grid {
                x,
                y,
                stroke_px: seed.last - seed.first + 1,
            };
            if !grids
                .iter()
                .any(|other| other.bounds().overlaps(grid.bounds()))
            {
                grids.push(grid);
            }
        }
    }
    grids
}

/// 标记已转换的矩形区域，给字边缘及表格边框留出少量余量。
fn mask_region(mask: &mut [u8], width: u32, height: u32, bounds: PixelRect, padding: u32) {
    let left = bounds.left.saturating_sub(padding) as usize;
    let right = (bounds.right + padding).min(width) as usize;
    for y in bounds.top.saturating_sub(padding)..(bounds.bottom + padding).min(height) {
        let row = y as usize * width as usize;
        mask[row + left..row + right].fill(1);
    }
}

/// 用稀疏采样估算纸色，使用亮度的第 85 百分位避开大部分墨迹。
fn paper_level(image: &GrayImage) -> u8 {
    let (width, height) = image.dimensions();
    let mut histogram = [0u32; 256];
    let mut samples = 0u32;
    for y in (0..height).step_by((height / 64).max(1) as usize) {
        for x in (0..width).step_by((width / 64).max(1) as usize) {
            histogram[image.get_pixel(x, y)[0] as usize] += 1;
            samples += 1;
        }
    }
    let mut seen = 0;
    histogram
        .iter()
        .position(|count| {
            seen += count;
            seen * 100 >= samples * 85
        })
        .map_or(255, |index| index as u8)
}

/// 用低分辨率网格聚合剩余墨迹，保留尺寸足够的连续图像区域。
fn picture_regions(image: &GrayImage, mask: &[u8], threshold: u8) -> Vec<PixelRect> {
    let (width, height) = image.dimensions();
    let tile = (width.min(height) / 70).clamp(10, 36);
    let cols = width.div_ceil(tile);
    let rows = height.div_ceil(tile);
    let mut active = vec![false; (cols * rows) as usize];
    for row in 0..rows {
        let top = row * tile;
        let bottom = (top + tile).min(height);
        for col in 0..cols {
            let left = col * tile;
            let right = (left + tile).min(width);
            let mut dark = 0;
            for y in top..bottom {
                for x in left..right {
                    let index = y as usize * width as usize + x as usize;
                    if mask[index] == 0 && image.get_pixel(x, y)[0] < threshold {
                        dark += 1;
                    }
                }
            }
            active[(row * cols + col) as usize] = dark * 7 >= (right - left) * (bottom - top);
        }
    }

    let mut pictures: Vec<PixelRect> = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let index = (row * cols + col) as usize;
            if !active[index] {
                continue;
            }
            active[index] = false;
            let mut stack = vec![(col, row)];
            let (mut left, mut top, mut right, mut bottom) = (col, row, col, row);
            let mut count = 0;
            while let Some((x, y)) = stack.pop() {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x);
                bottom = bottom.max(y);
                count += 1;
                for next_y in y.saturating_sub(1)..=(y + 1).min(rows - 1) {
                    for next_x in x.saturating_sub(1)..=(x + 1).min(cols - 1) {
                        let index = (next_y * cols + next_x) as usize;
                        if active[index] {
                            active[index] = false;
                            stack.push((next_x, next_y));
                        }
                    }
                }
            }
            if count >= 12 && right - left >= 2 && bottom - top >= 2 {
                let bounds = PixelRect {
                    left: (left * tile).saturating_sub(tile / 2),
                    top: (top * tile).saturating_sub(tile / 2),
                    right: ((right + 1) * tile + tile / 2).min(width),
                    bottom: ((bottom + 1) * tile + tile / 2).min(height),
                };
                merge_picture(&mut pictures, bounds);
            }
        }
    }
    pictures
}
