//! 将 PDF 页上的文字、表格和图片转换为 Word 元素。

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Cursor,
};

use anyhow::{Context, Result};
use docx_rs::{
    AbstractNumbering, AlignmentType, Docx, HeightRule, Hyperlink, HyperlinkType, IndentLevel,
    Level, LevelJc, LevelOverride, LevelText, LineSpacing, LineSpacingType, NumberFormat,
    Numbering, NumberingId, PageMargin, PageOrientationType, PageSize, Paragraph, ParagraphBorder,
    ParagraphBorderPosition, ParagraphBorders, Pic, Run, RunFonts, SectionProperty, Shading,
    SpecialIndentType, Start, StructuredDataTag, Style, StyleType, Tab, TabValueType, Table,
    TableBorder, TableBorderPosition, TableBorders, TableCell, TableCellMargins, TableLayoutType,
    TableRow, VMergeType, WidthType,
};
use pdf_oxide::{
    PdfDocument,
    annotation_types::AnnotationSubtype,
    annotations::{Annotation, LinkAction},
    elements::PathContent,
    extractors::SpanMergingConfig,
    geometry::Rect,
    layout::{TextChar, TextSpan},
    outline::{Destination, OutlineItem},
    rendering::{RenderOptions, render_page},
    structure::{
        McidScope, StructChild, StructElem, StructType, extract_table_from_spans_on_page,
        find_table_elements_all_pages,
        table_extractor::Table as PdfTable,
    },
};

use crate::{
    pdf_appearance::{self, FontFace},
    pdf_links::{PdfLinks, bookmark_name},
    raster_export::{encode_png, image_page_size},
    scan_ocr::{PixelRect, ScanElement, ScanLine, ScanOcr},
    scan_style::is_cjk,
    word_structure::{TABLE_CONTINUATION, WordStructure},
};

const TWIPS_PER_POINT: f32 = 20.0;
const EMUS_PER_POINT: f32 = 12_700.0;

enum WordBlock {
    Paragraph(Box<Paragraph>),
    Code(Box<Paragraph>, Option<usize>, bool),
    Table(Box<Table>, bool),
}

fn heading_key(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn outline_headings(pdf: &PdfDocument) -> HashMap<usize, HashMap<String, usize>> {
    fn collect(
        pdf: &PdfDocument,
        items: &[OutlineItem],
        level: usize,
        headings: &mut HashMap<usize, HashMap<String, usize>>,
    ) {
        for item in items {
            let page = match &item.dest {
                Some(Destination::PageIndex(page)) => Some(*page),
                Some(Destination::Named(name)) => {
                    pdf.resolve_named_destination(name).ok().flatten()
                }
                None => None,
            };
            let key = heading_key(&item.title);
            if let Some(page) = page
                && !key.is_empty()
            {
                headings.entry(page).or_default().insert(key, level.min(9));
            }
            collect(pdf, &item.children, level + 1, headings);
        }
    }
    let mut headings = HashMap::new();
    if let Ok(Some(items)) = pdf.get_outline() {
        collect(pdf, &items, 1, &mut headings);
    }
    headings
}

type PositionedBlock = (f32, f32, f32, WordBlock);

/// 记录使用文字识别和整页图片回退的原始页码。
#[derive(Default)]
pub struct PageReport {
    pub ocr_pages: Vec<usize>,
    pub rasterized_pages: Vec<usize>,
}

/// PDF 导出方式。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PdfMode {
    Auto,
    Ocr,
    Native,
    Images,
}

/// 判断页面是否有可用的文字层，允许正文夹带少量图标或未映射字形。
fn has_usable_text(spans: &[TextSpan]) -> bool {
    let mut readable = 0;
    let mut unmapped = 0;
    for character in spans.iter().flat_map(|span| span.text.chars()) {
        match character {
            '\u{fffd}'
            | '\u{e000}'..='\u{f8ff}'
            | '\u{f0000}'..='\u{ffffd}'
            | '\u{100000}'..='\u{10fffd}' => unmapped += 1,
            character if !character.is_whitespace() && !character.is_control() => readable += 1,
            _ => {}
        }
    }
    readable > 0 && unmapped <= readable / 4
}

struct PageStyle<'a> {
    fonts: &'a HashMap<String, String>,
    embedded: &'a HashMap<String, FontFace>,
    annotations: &'a [Annotation],
    anchors: &'a [Option<usize>],
    inline_pictures: &'a [(Rect, Pic)],
    code_chars: &'a [TextChar],
}

fn is_code_font(span: &TextSpan) -> bool {
    span.is_monospace || span.font_name.contains("CMUTypewriter")
}

/// 默认抽取会把部分代码里的反引号与相邻字母合成为重音字符。
/// 只用同一位置的原始等宽文字恢复反引号，其余内容保留默认抽取结果。
fn restore_code_backticks(pdf: &PdfDocument, page: usize, spans: &mut [TextSpan]) {
    let candidate = |span: &TextSpan| is_code_font(span) && !span.text.is_ascii();
    if !spans.iter().any(candidate) {
        return;
    }
    let Ok(original) = pdf.extract_spans_with_config(page, SpanMergingConfig::default()) else {
        return;
    };
    for span in spans.iter_mut().filter(|span| candidate(span)) {
        let backticks = span.text.matches('`').count();
        if let Some(raw) = original.iter().find(|raw| {
            raw.bbox == span.bbox
                && raw.font_name == span.font_name
                && raw.text.matches('`').count() > backticks
        }) {
            span.text.clone_from(&raw.text);
        }
    }
}

/// 私用区字形的含义由字体决定，保留 PDF 中的实际图形以便在 Word 中显示。
fn is_icon(span: &TextSpan) -> bool {
    let mut characters = span.text.chars();
    matches!(characters.next(), Some('\u{e000}'..='\u{f8ff}'))
        && characters.next().is_none()
        && span.font_name.contains("FontAwesome")
}

/// 过滤 XML 1.0 无法存储的字符，将控制类空白映射为空格。
fn clean_text(text: &str) -> String {
    text.chars()
        .filter_map(|character| match character {
            '\t' | '\n' | '\r' => Some(' '),
            '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}' => {
                Some(character)
            }
            _ => None,
        })
        .collect()
}

/// 将 RGB 三分量写为 Word 使用的六位十六进制颜色。
fn rgb_hex([red, green, blue]: [u8; 3]) -> String {
    format!("{red:02X}{green:02X}{blue:02X}")
}

/// 生成保留字号、字体、字重、斜体和颜色的 Word 文本片段。
fn styled_run(span: &TextSpan, font: &FontFace, text: &str, color: [u8; 3]) -> Run {
    let family = &font.family;
    let mut run = Run::new()
        .add_text(clean_text(text))
        .size((span.font_size * 2.0).round().clamp(2.0, 326.0) as usize)
        .color(rgb_hex(color));
    if !family.is_empty() {
        run = run.fonts(
            RunFonts::new()
                .ascii(family)
                .hi_ansi(family)
                .east_asia(family)
                .cs(family),
        );
    }
    if span.font_weight.is_bold() || font.bold {
        run = run.bold();
    }
    if span.is_italic || font.italic {
        run = run.italic();
    }
    run
}

/// 合并文字片段可能跨过颜色边界。只在原字形与现有文字匹配时恢复逐字颜色，
/// 继续使用已恢复的空格与文字顺序。
fn styled_runs(span: &TextSpan, font: &FontFace, chars: &[TextChar]) -> Vec<Run> {
    let fallback = || {
        vec![styled_run(
            span,
            font,
            &span.text,
            pdf_appearance::rgb(span.color),
        )]
    };
    if !is_code_font(span) || chars.is_empty() {
        return fallback();
    }
    let start = chars.partition_point(|ch| ch.origin_y < span.bbox.y - 1.0);
    let mut glyphs: Vec<_> = chars[start..]
        .iter()
        .take_while(|ch| ch.origin_y <= span.bbox.y + 1.0)
        .filter(|ch| {
            ch.font_name == span.font_name
                && ch.origin_x >= span.bbox.x - 0.1
                && ch.origin_x < span.bbox.x + span.bbox.width - 0.1
                && !ch.char.is_whitespace()
        })
        .collect();
    glyphs.sort_by(|a, b| a.origin_x.total_cmp(&b.origin_x));
    if !span
        .text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .eq(glyphs.iter().map(|ch| ch.char))
    {
        return fallback();
    }
    let mut runs = Vec::new();
    let mut start = 0;
    let mut color = pdf_appearance::rgb(span.color);
    let mut glyphs = glyphs.into_iter();
    for ((offset, _), glyph) in span
        .text
        .char_indices()
        .filter(|(_, ch)| !ch.is_whitespace())
        .zip(&mut glyphs)
    {
        let next_color = pdf_appearance::rgb(glyph.color);
        if next_color != color {
            if start < offset {
                runs.push(styled_run(span, font, &span.text[start..offset], color));
            }
            start = offset;
            color = next_color;
        }
    }
    runs.push(styled_run(span, font, &span.text[start..], color));
    runs
}

/// 根据 PDF 坐标聚合同行的文字，再按从左到右的顺序保留各段样式。
fn text_lines(mut spans: Vec<&TextSpan>) -> Vec<Vec<&TextSpan>> {
    spans.sort_by(|a, b| b.bbox.y.total_cmp(&a.bbox.y));
    let mut lines: Vec<Vec<&TextSpan>> = Vec::new();
    for span in spans {
        match lines.last_mut() {
            Some(line) if (line[0].bbox.y - span.bbox.y).abs() <= 2.5 => line.push(span),
            _ => lines.push(vec![span]),
        }
    }
    for line in &mut lines {
        line.sort_by(|a, b| a.bbox.x.total_cmp(&b.bbox.x));
    }
    lines
}

/// 检查批注矩形是否覆盖文字片段的大部分宽度和垂直中心。
fn annotation_covers(annotation: &Annotation, bounds: &Rect) -> bool {
    annotation.rect.is_some_and(|[left, bottom, right, top]| {
        let x = f64::from(bounds.x);
        let y = f64::from(bounds.y + bounds.height / 2.0);
        let width = f64::from(bounds.width);
        let overlap = (x + width).min(right) - x.max(left);
        y >= bottom && y <= top && overlap > width * 0.5
    })
}

fn hyperlink(style: &PageStyle<'_>, index: usize) -> Option<Hyperlink> {
    if let Some(Some(anchor)) = style.anchors.get(index) {
        return Some(Hyperlink::new(
            bookmark_name(*anchor),
            HyperlinkType::Anchor,
        ));
    }
    let Some(LinkAction::Uri(uri)) = &style.annotations[index].action else {
        return None;
    };
    let (scheme, _) = uri.split_once(':')?;
    ["https", "http", "mailto"]
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        .then(|| Hyperlink::new(uri, HyperlinkType::External))
}

/// PDF 链接的矩形可能只覆盖合并文字片段中的几个字。
fn linked_span_parts<'a>(
    span: &'a TextSpan,
    style: &PageStyle<'_>,
) -> Vec<(Cow<'a, TextSpan>, Option<usize>)> {
    let candidates: Vec<_> = style
        .annotations
        .iter()
        .enumerate()
        .filter_map(|(index, annotation)| {
            let [left, bottom, right, top] = annotation.rect?;
            let bounds = &span.bbox;
            let y = f64::from(bounds.y + bounds.height * 0.5);
            (y >= bottom
                && y <= top
                && f64::from(bounds.x) < right
                && f64::from(bounds.x + bounds.width) > left
                && hyperlink(style, index).is_some())
            .then_some(index)
        })
        .collect();
    if candidates.is_empty() {
        return vec![(Cow::Borrowed(span), None)];
    }
    let mut groups: Vec<(usize, Rect, Option<usize>)> = Vec::new();
    for ((offset, _), ch) in span.text.char_indices().zip(span.to_chars()) {
        let link = candidates
            .iter()
            .copied()
            .find(|&i| annotation_covers(&style.annotations[i], &ch.bbox));
        match groups.last_mut() {
            Some((_, bounds, current)) if *current == link => *bounds = bounds.union(&ch.bbox),
            _ => groups.push((offset, ch.bbox, link)),
        }
    }
    if groups.len() <= 1 {
        let link = groups.first().and_then(|group| group.2).or_else(|| {
            candidates
                .into_iter()
                .find(|&i| annotation_covers(&style.annotations[i], &span.bbox))
        });
        return vec![(Cow::Borrowed(span), link)];
    }
    groups
        .iter()
        .enumerate()
        .map(|(index, &(start, bounds, link))| {
            let end = groups.get(index + 1).map_or(span.text.len(), |next| next.0);
            let mut part = span.clone();
            part.text = span.text[start..end].to_owned();
            part.bbox = bounds;
            (Cow::Owned(part), link)
        })
        .collect()
}

/// 输出一行可编辑文字，恢复词间空格，并转换覆盖该片段的链接和文字标记。
fn line_paragraph(line: &[&TextSpan], style: &PageStyle<'_>) -> Paragraph {
    let mut paragraph = Paragraph::new().line_spacing(LineSpacing::new().after(0));
    let mut previous: Option<&TextSpan> = None;
    for span in line {
        let font = pdf_appearance::resolve_font(&span.font_name, style.fonts, style.embedded);
        if let Some(previous) = previous
            && let gap = span.bbox.x - previous.bbox.x - previous.bbox.width
            && gap > 1.5
            && !previous.text.ends_with(char::is_whitespace)
            && !span.text.starts_with(char::is_whitespace)
        {
            let space = if gap >= 8.0 && (span.is_monospace || font.family == "Consolas") {
                let count = (gap / (span.font_size * 0.55).max(1.0))
                    .round()
                    .clamp(1.0, 32.0);
                Run::new()
                    .fonts(RunFonts::new().ascii(&font.family).hi_ansi(&font.family))
                    .size((span.font_size * 2.0).round().clamp(2.0, 326.0) as usize)
                    .add_text(" ".repeat(count as usize))
            } else {
                Run::new().add_text(" ")
            };
            paragraph = paragraph.add_run(space);
        }
        for (part, link) in linked_span_parts(span, style) {
            let runs = if let Some((bounds, picture)) = style
                .inline_pictures
                .iter()
                .find(|(bounds, _)| *bounds == part.bbox)
            {
                let scale = EMUS_PER_POINT * 0.9;
                vec![Run::new().add_image(picture.clone().size(
                    (bounds.width * scale).round() as u32,
                    (bounds.height * scale).round() as u32,
                ))]
            } else {
                styled_runs(&part, &font, style.code_chars)
            };
            let mut link = link.and_then(|index| hyperlink(style, index));
            for mut run in runs {
                for annotation in style.annotations {
                    if !annotation_covers(annotation, &part.bbox) {
                        continue;
                    }
                    match annotation.subtype_enum {
                        AnnotationSubtype::Highlight => run = run.highlight("yellow"),
                        AnnotationSubtype::Underline => run = run.underline("single"),
                        AnnotationSubtype::StrikeOut => run = run.strike(),
                        _ => {}
                    }
                }
                if let Some(active) = link.take() {
                    // 同一链接可包含多个不同颜色的原生文字片段。
                    link = Some(active.add_run(run));
                } else {
                    paragraph = paragraph.add_run(run);
                }
            }
            if let Some(link) = link {
                paragraph = paragraph.add_hyperlink(link);
            }
        }
        previous = Some(span);
    }
    paragraph
}

/// 仅当文字已经出现在某个单元格中时才从正文去重。
///
/// 表格检测可能漏掉合并区域里的文字，不能仅按整个表格的外框过滤。
fn already_in_table(span: &TextSpan, table: &PdfTable) -> bool {
    let x = span.bbox.x + span.bbox.width / 2.0;
    let y = span.bbox.y + span.bbox.height / 2.0;
    let contains_center = |bounds: Rect| {
        x >= bounds.x
            && x <= bounds.x + bounds.width
            && y >= bounds.y
            && y <= bounds.y + bounds.height
    };
    table.bbox.is_some_and(contains_center)
        && table.rows.iter().flat_map(|row| &row.cells).any(|cell| {
            cell.bbox.is_some_and(contains_center)
                && (cell.text.contains(span.text.trim())
                    || cell.spans.iter().any(|source| {
                        source.mcid.is_some()
                            && source.mcid == span.mcid
                            && source.mcid_scope == span.mcid_scope
                            && source.bbox == span.bbox
                            && source.text == span.text
                    }))
        })
}

/// 目前只在标签能完整表示普通网格时使用它。跨流引用或嵌套表格交给几何检测。
fn supports_table_tags(element: &StructElem, page: u32) -> bool {
    element.children.iter().all(|child| match child {
        StructChild::StructElem(child) => {
            child.struct_type != StructType::Table && supports_table_tags(child, page)
        }
        StructChild::MarkedContentRef {
            page: source, scope, ..
        } => *source != page || *scope == McidScope::Page(page),
        StructChild::ObjectRef(..) => true,
    })
}

/// 优先使用 PDF 的逻辑行列，按当前页的标记编号取回带样式的原始文字。
fn tagged_tables(elements: &[StructElem], page: u32, spans: &[&TextSpan]) -> Vec<PdfTable> {
    let mut by_mcid = HashMap::<u32, Vec<&TextSpan>>::new();
    for &span in spans {
        if let Some(mcid) = span.mcid
            && (span.mcid_scope.is_none() || span.mcid_scope == Some(McidScope::Page(page)))
        {
            by_mcid.entry(mcid).or_default().push(span);
        }
    }
    elements
        .iter()
        .filter(|element| supports_table_tags(element, page))
        .filter_map(|element| {
            // 只从标签取行列和编号，保留原始 span 的字体、颜色、位置和间距。
            let mut table = extract_table_from_spans_on_page(element, &[], Some(page)).ok()?;
            if table.col_count < 2
                || table.rows.iter().any(|row| row.cells.len() != table.col_count)
            {
                return None;
            }
            let mut used = HashSet::new();
            for row in &mut table.rows {
                row.is_header |= !row.cells.is_empty() && row.cells.iter().all(|cell| cell.is_header);
                for cell in &mut row.cells {
                    cell.mcids.sort_unstable();
                    cell.mcids.dedup();
                    for mcid in &cell.mcids {
                        if !used.insert(*mcid) {
                            return None;
                        }
                        if let Some(sources) = by_mcid.get(mcid) {
                            cell.spans.extend(sources.iter().map(|&span| span.clone()));
                        }
                    }
                    if cell.spans.is_empty() && !cell.mcids.is_empty() {
                        return None;
                    }
                    cell.text = cell.spans.iter().map(|span| span.text.as_str()).collect();
                    cell.bbox = cell.spans.iter().map(|span| span.bbox).reduce(|a, b| a.union(&b));
                }
            }
            table.bbox = table
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .filter_map(|cell| cell.bbox)
                .reduce(|a, b| a.union(&b));
            table.bbox.is_some().then_some(table)
        })
        .collect()
}

/// 将识别出的网格线颜色和线宽应用到 Word 表格的外框及内部边线。
fn grid_borders(color: [u8; 3], width_pt: f32) -> TableBorders {
    let color = rgb_hex(color);
    let size = (width_pt * 8.0).round().clamp(2.0, 96.0) as usize;
    [
        TableBorderPosition::Top,
        TableBorderPosition::Bottom,
        TableBorderPosition::Left,
        TableBorderPosition::Right,
        TableBorderPosition::InsideH,
        TableBorderPosition::InsideV,
    ]
    .into_iter()
    .fold(TableBorders::new(), |borders, position| {
        borders.set(TableBorder::new(position).color(&color).size(size))
    })
}

/// 将检测出的表格写成 Word 网格，保留单元格样式、底色及横向和纵向合并。
fn word_table(
    table: &PdfTable,
    style: &PageStyle<'_>,
    width_pt: f32,
    paths: &[PathContent],
    bounds: Rect,
) -> Option<Table> {
    let width = (width_pt * TWIPS_PER_POINT).round() as usize;
    let column_count = table.col_count.max(1);
    let mut edges = pdf_appearance::table_columns(paths, bounds, column_count);
    if edges.is_empty() {
        // 无竖线的表格以各列靠左的单元格为锚点，保留原有列宽比例。
        let mut starts = vec![f32::INFINITY; column_count];
        for row in &table.rows {
            if row.cells.len() != column_count || row.cells.iter().any(|cell| cell.colspan > 1) {
                continue;
            }
            for (start, cell) in starts.iter_mut().zip(&row.cells) {
                if let Some(cell_bounds) = cell.bbox {
                    *start = start.min(cell_bounds.x);
                }
            }
        }
        starts.push(bounds.x + width_pt);
        if starts.windows(2).all(|pair| pair[1] - pair[0] >= 8.0) {
            edges = starts;
        }
    }
    let column_widths: Vec<usize> = if let (Some(left), Some(right)) = (edges.first(), edges.last())
    {
        edges
            .windows(2)
            .map(|pair| {
                ((pair[1] - pair[0]) / (right - left) * width as f32)
                    .round()
                    .max(1.0) as usize
            })
            .collect()
    } else {
        vec![(width / column_count).max(1); column_count]
    };
    let row_height = (bounds.height / table.rows.len().max(1) as f32).clamp(1.0, 48.0);
    let new_cell = |column: usize, columns: usize| {
        let width = column_widths[column..column + columns].iter().sum();
        let cell = TableCell::new().width(width, WidthType::Dxa);
        if columns > 1 {
            cell.grid_span(columns)
        } else {
            cell
        }
    };
    let mut rows = Vec::new();
    let mut merges = vec![None; column_count];
    for (row_index, row) in table.rows.iter().enumerate() {
        let mut cells = Vec::new();
        let mut source_cells = row.cells.iter();
        let mut column = 0;
        while column < column_count {
            // 检测器会省略被上方单元格覆盖的位置，Word 需要显式的续接单元格。
            if let Some((columns, end_row)) = merges[column]
                && row_index < end_row
            {
                cells.push(
                    new_cell(column, columns)
                        .vertical_merge(VMergeType::Continue)
                        .add_paragraph(Paragraph::new()),
                );
                column += columns;
                continue;
            }
            let Some(source) = source_cells.next() else {
                cells.push(new_cell(column, 1).add_paragraph(Paragraph::new()));
                column += 1;
                continue;
            };
            let columns = (source.colspan as usize).max(1);
            if columns > column_count - column
                || merges[column + 1..column + columns]
                    .iter()
                    .flatten()
                    .any(|&(_, end_row)| row_index < end_row)
            {
                return None;
            }
            let mut cell = new_cell(column, columns);
            if let Some(cell_bounds) = source.bbox
                && let Some(fill) = pdf_appearance::cell_fill(
                    paths,
                    bounds,
                    cell_bounds.x + cell_bounds.width / 2.0,
                    cell_bounds.y + cell_bounds.height / 2.0,
                )
            {
                cell = cell.shading(Shading::new().fill(rgb_hex(fill)));
            }
            if source.rowspan > 1 {
                let end = row_index.saturating_add(source.rowspan as usize);
                merges[column] = Some((columns, end));
                cell = cell.vertical_merge(VMergeType::Restart);
            }
            if source.spans.is_empty() {
                let text = clean_text(&source.text);
                cell = cell.add_paragraph(Paragraph::new().add_run(Run::new().add_text(text)));
            } else {
                for line in text_lines(source.spans.iter().collect()) {
                    cell = cell.add_paragraph(line_paragraph(&line, style));
                }
            }
            cells.push(cell);
            column += columns;
        }
        if source_cells.next().is_some() {
            return None;
        }
        rows.push(
            TableRow::new(cells)
                .row_height((row_height * TWIPS_PER_POINT).round())
                .height_rule(HeightRule::AtLeast)
                .cant_split(),
        );
    }
    let mut result = Table::new(rows)
        .set_grid(column_widths)
        .width(width, WidthType::Dxa)
        .margins(TableCellMargins::new().margin(45, 100, 45, 100))
        .layout(TableLayoutType::Fixed);
    if let Some((color, stroke)) = pdf_appearance::table_border(paths, bounds) {
        result = result.set_borders(grid_borders(color, stroke));
    }
    Some(result)
}

/// 等比例缩放图片，使其可以放入当前 Word 页的正文区域。
fn image_paragraph(
    mut image: Pic,
    width: f32,
    height: f32,
    content_width: f32,
    content_height: f32,
) -> Paragraph {
    let scale = (content_width / width)
        .min(content_height / height)
        .min(1.0);
    image = image.size(
        (width * scale * EMUS_PER_POINT).max(1.0) as u32,
        (height * scale * EMUS_PER_POINT).max(1.0) as u32,
    );
    Paragraph::new()
        .line_spacing(LineSpacing::new().after(0))
        .add_run(Run::new().add_image(image))
}

/// 将估算出的字形属性应用到 OCR 文字。
fn ocr_run(line: &ScanLine, scale: f32) -> Run {
    let mut run = Run::new()
        .size(
            (line.style.size_px * scale * 2.0)
                .round()
                .clamp(12.0, 144.0) as usize,
        )
        .color(rgb_hex(line.style.rgb))
        .fonts(
            RunFonts::new()
                .ascii("Calibri")
                .hi_ansi("Calibri")
                .east_asia("Microsoft YaHei"),
        )
        .add_text(clean_text(&line.text));
    if line.style.bold {
        run = run.bold();
    }
    if line.style.italic {
        run = run.italic();
    }
    run
}

/// 根据文字块在纸张上的水平位置恢复常见的居中和靠右对齐。
fn visual_alignment(x: f32, width: f32, page_width: f32, margin: f32) -> AlignmentType {
    let right = page_width - x - width;
    if x > margin + 12.0 && (x - right).abs() <= 9.0 {
        AlignmentType::Center
    } else if right <= margin + 12.0 && x > page_width * 0.5 {
        AlignmentType::Right
    } else {
        AlignmentType::Left
    }
}

/// 将识别出来的文字、网格和插图转为可写入 Word 的页面元素。
fn ocr_blocks(
    engine: &mut ScanOcr,
    image: &image::RgbImage,
    origin: (f32, f32),
    page_size: (f32, f32),
    available: (f32, f32),
    picture_hints: &[PixelRect],
) -> Result<Vec<PositionedBlock>> {
    let elements = engine.recognize(image, picture_hints)?;
    let scale_x = page_size.0 / image.width() as f32;
    let scale_y = page_size.1 / image.height() as f32;
    let margin = (page_size.0 - available.0) / 2.0;
    let mut font_sizes = Vec::new();
    for (_, element) in &elements {
        if let ScanElement::Text(lines) = element {
            font_sizes.extend(lines.iter().map(|line| line.style.size_px));
        }
    }
    font_sizes.sort_by(f32::total_cmp);
    let body_size = font_sizes.get(font_sizes.len() / 2).copied();
    let mut blocks = Vec::new();
    for (bounds, element) in elements {
        let x = bounds.left as f32 * scale_x;
        let width = bounds.width() as f32 * scale_x;
        let height = bounds.height() as f32 * scale_y;
        let indent = ((x - margin).clamp(0.0, (available.0 - width).max(0.0)) * TWIPS_PER_POINT)
            .round() as i32;
        let block = match element {
            ScanElement::Text(lines) => {
                let mut paragraph = Paragraph::new();
                if let Some(first) = lines.first() {
                    let alignment = visual_alignment(x, width, page_size.0, margin);
                    let offset = first.bounds.left.saturating_sub(bounds.left) as f32 * scale_x;
                    let first_line = (offset >= 2.0).then_some(SpecialIndentType::FirstLine(
                        (offset * TWIPS_PER_POINT).round() as i32,
                    ));
                    paragraph = paragraph.align(alignment);
                    if matches!(alignment, AlignmentType::Left) {
                        paragraph = paragraph.indent(Some(indent), first_line, None, None);
                    }
                    if lines.len() == 1
                        && first.text.chars().count() < 100
                        && body_size.is_some_and(|size| first.style.size_px > size * 1.28)
                    {
                        paragraph = paragraph.style("Heading1").outline_lvl(0).keep_next(true);
                    }
                }
                let mut last: Option<char> = None;
                for line in &lines {
                    let first = line.text.chars().next();
                    if let (Some(previous), Some(next)) = (last, first)
                        && !(previous.is_whitespace()
                            || next.is_whitespace()
                            || is_cjk(previous) && is_cjk(next))
                    {
                        paragraph = paragraph.add_run(Run::new().add_text(" "));
                    }
                    paragraph = paragraph.add_run(ocr_run(line, scale_y));
                    last = line.text.chars().next_back();
                }
                WordBlock::Paragraph(Box::new(paragraph))
            }
            ScanElement::Table {
                column_widths,
                rows,
                fills,
                border_rgb,
                border_px,
            } => {
                // 用扫描网格的相对列宽生成原生单元格，文字按坐标落入对应列。
                let grid: Vec<_> = column_widths
                    .iter()
                    .map(|&pixels| {
                        (pixels as f32 / bounds.width() as f32
                            * width.min(available.0)
                            * TWIPS_PER_POINT)
                            .round()
                            .max(1.0) as usize
                    })
                    .collect();
                let rows = rows
                    .into_iter()
                    .zip(fills)
                    .map(|(row, fills)| {
                        let cells = row.into_iter().zip(&grid).zip(fills).map(
                            |((lines, &column_width), fill)| {
                                let mut cell = TableCell::new().width(column_width, WidthType::Dxa);
                                if fill != [255; 3] {
                                    cell = cell.shading(Shading::new().fill(rgb_hex(fill)));
                                }
                                if lines.is_empty() {
                                    return cell.add_paragraph(Paragraph::new());
                                }
                                for line in lines {
                                    cell = cell.add_paragraph(
                                        Paragraph::new()
                                            .line_spacing(LineSpacing::new().after(0))
                                            .add_run(ocr_run(&line, scale_y)),
                                    );
                                }
                                cell
                            },
                        );
                        TableRow::new(cells.collect()).cant_split()
                    })
                    .collect();
                let width = grid.iter().sum();
                WordBlock::Table(
                    Box::new(
                        Table::new(rows)
                            .set_grid(grid)
                            .width(width, WidthType::Dxa)
                            .indent(indent)
                            .set_borders(grid_borders(border_rgb, border_px as f32 * scale_y))
                            .margins(TableCellMargins::new().margin(45, 90, 45, 90))
                            .layout(TableLayoutType::Fixed),
                    ),
                    false,
                )
            }
            ScanElement::Picture => {
                // 裁切原始彩色像素，保持插图可移动和缩放。
                let picture = cropped_picture(
                    image,
                    bounds.left,
                    bounds.top,
                    bounds.width(),
                    bounds.height(),
                )?;
                WordBlock::Paragraph(Box::new(
                    image_paragraph(picture, width, height, available.0, available.1).indent(
                        Some(indent),
                        None,
                        None,
                        None,
                    ),
                ))
            }
        };
        blocks.push((
            origin.1 - bounds.top as f32 * scale_y,
            origin.0 + x,
            origin.1 - bounds.bottom as f32 * scale_y,
            block,
        ));
    }
    Ok(blocks)
}

/// 将页面栅格的一个区域编码为 Word 的独立图片。
fn cropped_picture(
    image: &image::RgbImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<Pic> {
    let crop = image::imageops::crop_imm(image, x, y, width, height).to_image();
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(crop).write_to(&mut bytes, image::ImageFormat::Png)?;
    Ok(Pic::new_with_dimensions(bytes.into_inner(), width, height))
}

/// 同页矢量图只渲染一次，再将各个区域按其原位置裁出。
fn vector_pictures(
    pdf: &PdfDocument,
    page: usize,
    media: Rect,
    bounds: &[Rect],
    options: &RenderOptions,
) -> Result<Vec<(Rect, Pic)>> {
    if bounds.is_empty() || pdf.get_page_rotation(page)?.rem_euclid(360) != 0 {
        return Ok(Vec::new());
    }
    let raster = render_page(pdf, page, options)?;
    let raster =
        image::load_from_memory_with_format(&raster.data, image::ImageFormat::Png)?.into_rgb8();
    let scale_x = raster.width() as f32 / media.width;
    let scale_y = raster.height() as f32 / media.height;
    let mut pictures = Vec::new();
    for &region in bounds {
        let x = ((region.x - media.x) * scale_x).floor() as u32;
        let y = ((media.y + media.height - region.y - region.height) * scale_y).floor() as u32;
        let width = ((region.width * scale_x).ceil() as u32).min(raster.width().saturating_sub(x));
        let height =
            ((region.height * scale_y).ceil() as u32).min(raster.height().saturating_sub(y));
        if width > 0 && height > 0 {
            pictures.push((region, cropped_picture(&raster, x, y, width, height)?));
        }
    }
    Ok(pictures)
}

/// 创建含 Word 标题样式的文档。
fn new_document() -> Docx {
    let mut document = Docx::new().default_line_spacing(LineSpacing::new().before(0).after(0));
    for level in 1..=9 {
        document = document.add_style(
            Style::new(&format!("Heading{level}"), StyleType::Paragraph)
                .name(format!("heading {level}"))
                .based_on("Normal")
                .next("Normal")
                .outline_lvl(level - 1),
        );
    }
    document
}

/// 按源位置排序并写入 Word，限制较大的空白以减少额外分页。
fn append_blocks(
    mut document: Docx,
    mut blocks: Vec<PositionedBlock>,
    top: f32,
    margin: f32,
    bookmarks: &[(usize, Option<f32>)],
) -> Docx {
    blocks.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.total_cmp(&b.1)));
    let mut bookmarks_at = HashMap::<usize, Vec<usize>>::new();
    for &(id, y) in bookmarks {
        let index = y
            .and_then(|y| {
                blocks
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| (a.0 - y).abs().total_cmp(&(b.0 - y).abs()))
                    .map(|(index, _)| index)
            })
            .unwrap_or(0);
        bookmarks_at.entry(index).or_default().push(id);
    }
    let mut previous_bottom = top - margin;
    let block_count = blocks.len();
    let mut code = None::<((Option<usize>, bool), StructuredDataTag)>;
    for (index, (block_top, _, block_bottom, block)) in blocks.into_iter().enumerate() {
        let code_kind = match &block {
            WordBlock::Code(_, id, output) => Some((*id, *output)),
            _ => None,
        };
        if code.as_ref().is_some_and(|(id, _)| {
            Some(*id) != code_kind
                || previous_bottom - block_top > 12.0
                || bookmarks_at.contains_key(&index)
        }) && let Some((_, group)) = code.take()
        {
            document = document.add_structured_data_tag(group);
        }
        for id in bookmarks_at.remove(&index).into_iter().flatten() {
            document = document
                .add_bookmark_start(id, bookmark_name(id))
                .add_bookmark_end(id);
        }
        let gap =
            ((previous_bottom - block_top - 4.0).clamp(0.0, 48.0) * TWIPS_PER_POINT).round() as u32;
        previous_bottom = previous_bottom.min(block_bottom);
        document = match block {
            WordBlock::Code(paragraph, id, output) => {
                let (_, group) = code.take().unwrap_or_else(|| {
                    (
                        (id, output),
                        StructuredDataTag::new().alias(if output {
                            "Program output"
                        } else {
                            "Code block"
                        }),
                    )
                });
                let spacing = paragraph.property.line_spacing.clone().unwrap_or_default();
                code = Some((
                    (id, output),
                    group.add_paragraph(paragraph.line_spacing(spacing.before(gap).after(0))),
                ));
                document
            }
            WordBlock::Paragraph(paragraph) => {
                let spacing = paragraph
                    .property
                    .line_spacing
                    .clone()
                    .unwrap_or_default()
                    .before(gap)
                    .after(0);
                document.add_paragraph(paragraph.line_spacing(spacing))
            }
            WordBlock::Table(table, repeat_header) => {
                if gap > 0 {
                    document = document.add_paragraph(
                        Paragraph::new().size(2).line_spacing(
                            LineSpacing::new()
                                .line(gap as i32)
                                .line_rule(LineSpacingType::Exact)
                                .after(0),
                        ),
                    );
                }
                if repeat_header {
                    document.add_structured_data_tag(
                        StructuredDataTag::new()
                            .alias(TABLE_CONTINUATION)
                            .add_table(*table),
                    )
                } else {
                    document.add_table(*table)
                }
            }
        };
    }
    if let Some((_, group)) = code {
        document = document.add_structured_data_tag(group);
    }
    for id in bookmarks_at.remove(&block_count).into_iter().flatten() {
        document = document
            .add_bookmark_start(id, bookmark_name(id))
            .add_bookmark_end(id);
    }
    document
}

/// 将无法识别的页面或选择按原样保留的页面按比例插入 Word。
fn full_page_picture(
    png: Vec<u8>,
    pixels: (u32, u32),
    available: (f32, f32),
    top: f32,
    bottom: f32,
) -> PositionedBlock {
    let scale = (available.0 / pixels.0 as f32).min(available.1 / pixels.1 as f32);
    let picture = Pic::new_with_dimensions(png, pixels.0, pixels.1);
    let paragraph = image_paragraph(
        picture,
        pixels.0 as f32 * scale,
        pixels.1 as f32 * scale,
        available.0,
        available.1,
    )
    .align(AlignmentType::Center);
    (top, 0.0, bottom, WordBlock::Paragraph(Box::new(paragraph)))
}

/// 接收一张已经解码的图片，使用与 PDF 页面相同的 OCR 和 Word 排版逻辑。
pub(crate) fn build_image_document(
    image: &image::RgbImage,
    mode: PdfMode,
) -> Result<(Docx, PageReport)> {
    let (width, height) = image_page_size(image)?;
    let (width, height) = (width.max(72.0), height.max(72.0));
    let margin = if mode == PdfMode::Images {
        0.0
    } else {
        36.0_f32.min(width.min(height) / 6.0)
    };
    let available = (width - margin * 2.0, height - margin * 2.0 - 16.0);
    let mut blocks = if matches!(mode, PdfMode::Auto | PdfMode::Ocr) {
        ocr_blocks(
            &mut ScanOcr::new()?,
            image,
            (0.0, height),
            (width, height),
            available,
            &[],
        )?
    } else {
        Vec::new()
    };
    let mut report = PageReport::default();
    if blocks.is_empty() {
        blocks.push(full_page_picture(
            encode_png(image)?,
            image.dimensions(),
            available,
            height,
            0.0,
        ));
        report.rasterized_pages.push(1);
    } else {
        report.ocr_pages.push(1);
    }
    let margin = (margin * TWIPS_PER_POINT).round() as i32;
    let orientation = if width > height {
        PageOrientationType::Landscape
    } else {
        PageOrientationType::Portrait
    };
    let document = append_blocks(
        new_document(),
        blocks,
        height,
        margin as f32 / TWIPS_PER_POINT,
        &[],
    )
    .page_size(
        (width * TWIPS_PER_POINT).round() as u32,
        (height * TWIPS_PER_POINT).round() as u32,
    )
    .page_orient(orientation)
    .page_margin(
        PageMargin::new()
            .top(margin)
            .bottom(margin)
            .left(margin)
            .right(margin),
    );
    Ok((document, report))
}

/// 逐页构建 Word，自动模式优先读取原文字层，缺失或无法解码时使用 OCR。
pub(crate) fn build_document(
    pdf: &PdfDocument,
    pages: &[usize],
    mode: PdfMode,
    dpi: u32,
) -> Result<(docx_rs::XMLDocx, PageReport)> {
    let use_text_layer = matches!(mode, PdfMode::Auto | PdfMode::Native);
    let font_faces = if use_text_layer {
        pdf.page_font_face_lookups().unwrap_or_else(|error| {
            eprintln!("提示：无法读取 PDF 字体信息：{error}");
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let no_fonts = HashMap::new();
    let embedded = if use_text_layer {
        pdf_appearance::embedded_faces(pdf)
    } else {
        HashMap::new()
    };
    let mut document = new_document();
    let mut report = PageReport::default();
    let mut engine = None;
    let render_options = RenderOptions::with_dpi(dpi);
    let ocr_render_options = RenderOptions::with_dpi(dpi.max(200));
    let links = if use_text_layer {
        PdfLinks::read(pdf, pages)
    } else {
        PdfLinks::default()
    };
    let headings = if use_text_layer {
        outline_headings(pdf)
    } else {
        HashMap::new()
    };
    let table_tags = if use_text_layer {
        pdf.structure_tree()
            .ok()
            .flatten()
            .map(|tree| find_table_elements_all_pages(&tree))
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut next_code_id = 1;
    let mut structure = WordStructure::default();
    let mut previous_tables = Vec::<(usize, Rect, Vec<String>)>::new();
    document = document.add_abstract_numbering(
        AbstractNumbering::new(1).add_level(
            Level::new(
                0,
                Start::new(1),
                NumberFormat::new("decimal"),
                LevelText::new("%1"),
                LevelJc::new("right"),
            )
            .size(18),
        ),
    );

    for (output_index, &page) in pages.iter().enumerate() {
        let mut next_tables = Vec::new();
        let recognize_pictures = mode == PdfMode::Auto;
        let fonts = font_faces.get(page).map_or(&no_fonts, |fonts| fonts);
        let (left, bottom, right, top) = pdf.get_page_media_box(page)?;
        let spans = if use_text_layer {
            match pdf.extract_spans(page) {
                Ok(mut spans) => {
                    restore_code_backticks(pdf, page, &mut spans);
                    spans
                }
                Err(error) if mode == PdfMode::Auto => {
                    eprintln!(
                        "提示：无法读取第 {} 页的文字层，将使用 OCR：{error}",
                        page + 1
                    );
                    Vec::new()
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("无法读取第 {} 页的文字层", page + 1));
                }
            }
        } else {
            Vec::new()
        };
        let mode = if mode == PdfMode::Auto {
            if has_usable_text(&spans) {
                PdfMode::Native
            } else {
                PdfMode::Ocr
            }
        } else {
            mode
        };
        let spans: Vec<_> = spans
            .iter()
            .filter(|span| mode == PdfMode::Native && !span.text.trim().is_empty())
            .collect();

        // Word 的页面尺寸存在上限，小型标签页则需要更窄的边距。
        let mut width = (right - left).abs().clamp(72.0, 1584.0);
        let mut height = (top - bottom).abs().clamp(72.0, 1584.0);
        if spans.is_empty() && pdf.get_page_rotation(page)?.rem_euclid(180) == 90 {
            std::mem::swap(&mut width, &mut height);
        }
        let margin_pt = match mode {
            PdfMode::Images => 0.0,
            PdfMode::Native => 24.0_f32.min(width.min(height) / 6.0),
            _ => 36.0_f32.min(width.min(height) / 6.0),
        };
        let content_width = width - 2.0 * margin_pt;
        let content_height = height - 2.0 * margin_pt - 16.0;
        let content_left = left + margin_pt;
        let left_indent = |x: f32, block_width: f32| {
            let max_indent = (content_width - block_width).max(0.0);
            ((x - content_left).clamp(0.0, max_indent) * TWIPS_PER_POINT).round() as i32
        };
        let margin_twips = (margin_pt * TWIPS_PER_POINT).round() as i32;
        let margin = PageMargin::new()
            .top(margin_twips)
            .bottom(margin_twips)
            .left(margin_twips)
            .right(margin_twips);
        let orientation = if width > height {
            PageOrientationType::Landscape
        } else {
            PageOrientationType::Portrait
        };
        let page_width = (width * TWIPS_PER_POINT).round() as u32;
        let page_height = (height * TWIPS_PER_POINT).round() as u32;

        // 从同一页提取各类内容，再用源坐标恢复正文、表格和图片的先后顺序。
        let images = if mode == PdfMode::Native {
            pdf.extract_images(page)
        } else {
            Ok(Vec::new())
        };
        let mut blocks = Vec::new();

        if spans.is_empty() {
            // OCR 和整页图片模式都将每个选中页渲染成图片。
            let has_images = !images.as_ref().is_ok_and(Vec::is_empty);
            if mode != PdfMode::Native || has_images || !pdf.extract_paths(page)?.is_empty() {
                let options = if mode == PdfMode::Ocr {
                    &ocr_render_options
                } else {
                    &render_options
                };
                let image = render_page(pdf, page, options)
                    .with_context(|| format!("无法渲染 PDF 第 {} 页", page + 1))?;
                if mode == PdfMode::Ocr {
                    let raster =
                        image::load_from_memory_with_format(&image.data, image::ImageFormat::Png)?
                            .into_rgb8();
                    let media = Rect::new(left, bottom, (right - left).abs(), (top - bottom).abs());
                    let picture_hints = pdf_appearance::picture_pixel_regions(
                        pdf,
                        page,
                        media,
                        raster.dimensions(),
                    );
                    if engine.is_none() {
                        engine = Some(ScanOcr::new()?);
                    }
                    if let Some(engine) = &mut engine {
                        blocks = ocr_blocks(
                            engine,
                            &raster,
                            (left, top),
                            (width, height),
                            (content_width, content_height),
                            &picture_hints,
                        )
                        .with_context(|| format!("无法识别 PDF 第 {} 页", page + 1))?;
                    }
                }
                if blocks.is_empty() {
                    blocks.push(full_page_picture(
                        image.data,
                        (image.width, image.height),
                        (content_width, content_height),
                        top,
                        bottom,
                    ));
                    report.rasterized_pages.push(page + 1);
                } else {
                    report.ocr_pages.push(page + 1);
                }
            }
        } else {
            let style = PageStyle {
                fonts,
                embedded: &embedded,
                annotations: links.annotations.get(&page).map_or(&[], Vec::as_slice),
                anchors: links.anchors.get(&page).map_or(&[], Vec::as_slice),
                inline_pictures: &[],
                code_chars: &[],
            };
            let tagged = tagged_tables(
                table_tags.get(&(page as u32)).map_or(&[], Vec::as_slice),
                page as u32,
                &spans,
            );
            let spatial = pdf.extract_tables(page)?;
            let paths = pdf.extract_paths(page)?;
            let mut code_chars = if spans.iter().any(|span| is_code_font(span)) {
                pdf.extract_chars(page).unwrap_or_default()
            } else {
                Vec::new()
            };
            code_chars.retain(|ch| ch.origin_y.is_finite());
            code_chars.sort_by(|a, b| a.origin_y.total_cmp(&b.origin_y));
            let mut exported_tables = Vec::new();
            let mut exported_tagged: Vec<(Rect, &PdfTable)> = Vec::new();
            for (table, from_tags) in tagged
                .iter()
                .map(|table| (table, true))
                .chain(spatial.iter().map(|table| (table, false)))
            {
                if let Some(bounds) = table.bbox {
                    if !from_tags && exported_tagged.iter().any(|(tagged, _)| {
                        tagged.intersection(&bounds).is_some_and(|overlap| {
                            overlap.area() > tagged.area().min(bounds.area()) * 0.5
                        })
                    }) {
                        continue;
                    }
                    // 检测框通常以文字边缘为界，给末列补足单元格边距。
                    let table_width = (bounds.width + 24.0).clamp(24.0, content_width);
                    if let Some(word_table) = word_table(table, &style, table_width, &paths, bounds)
                    {
                        let header: Vec<_> = table
                            .rows
                            .first()
                            .into_iter()
                            .flat_map(|row| &row.cells)
                            .map(|cell| heading_key(&cell.text))
                            .collect();
                        let continues = table.rows.len() >= 2
                            && header.len() >= 2
                            && header.iter().all(|cell| !cell.is_empty())
                            && top - bounds.y - bounds.height <= height * 0.25
                            && previous_tables.iter().any(|(previous, rect, labels)| {
                                previous + 1 == page
                                    && labels == &header
                                    && (rect.x - bounds.x).abs() <= 6.0
                                    && (rect.width - bounds.width).abs() <= width * 0.06
                            });
                        if bounds.y - bottom <= height * 0.25 {
                            next_tables.push((page, bounds, header));
                        }
                        blocks.push((
                            bounds.y + bounds.height,
                            bounds.x,
                            bounds.y,
                            WordBlock::Table(
                                Box::new(
                                    word_table.indent(left_indent(bounds.x - 5.0, table_width)),
                                ),
                                continues || table.rows.first().is_some_and(|row| row.is_header),
                            ),
                        ));
                        exported_tables.push(table);
                        if from_tags {
                            exported_tagged.push((bounds, table));
                        }
                    }
                }
            }
            let media = Rect::new(left, bottom, (right - left).abs(), (top - bottom).abs());
            let table_regions: Vec<_> = exported_tables
                .iter()
                .filter_map(|table| table.bbox)
                .collect();
            let mut vector_regions =
                pdf_appearance::vector_regions(&paths, &table_regions, &spans, media);
            // 表格裁图可能延伸至末列的空白，超出按文字计算的表格范围。
            vector_regions.retain(|region| {
                !exported_tagged.iter().any(|(bounds, table)| {
                    bounds.intersects(region)
                        && table.rows.iter().flat_map(|row| &row.cells).any(|cell| {
                            cell.spans.iter().any(|span| region.contains_rect(&span.bbox))
                        })
                })
            });
            let icon_regions: Vec<_> = spans
                .iter()
                .filter(|span| is_icon(span))
                .filter(|span| {
                    media.contains_rect(&span.bbox)
                        && span.bbox.width > 0.0
                        && span.bbox.height > 0.0
                })
                .map(|span| span.bbox)
                .collect();
            vector_regions.extend(icon_regions.iter().copied());
            let vector_images =
                match vector_pictures(pdf, page, media, &vector_regions, &render_options) {
                    Ok(images) => images,
                    Err(error) => {
                        eprintln!("提示：第 {} 页的矢量插图无法渲染：{error}", page + 1);
                        Vec::new()
                    }
                };
            let (inline_pictures, vector_images): (Vec<_>, Vec<_>) = vector_images
                .into_iter()
                .partition(|(bounds, _)| icon_regions.contains(bounds));
            let style = PageStyle {
                inline_pictures: &inline_pictures,
                code_chars: &code_chars,
                ..style
            };
            let mut sizes: Vec<_> = spans.iter().map(|span| span.font_size).collect();
            let body = spans.iter().copied().filter(|span| {
                !exported_tables
                    .iter()
                    .any(|table| already_in_table(span, table))
                    && !vector_images
                        .iter()
                        .any(|(bounds, _)| bounds.contains_rect(&span.bbox))
            });
            sizes.sort_by(f32::total_cmp);
            let typical_size = sizes.get(sizes.len() / 2).copied();
            let mut code_numbering = None::<(usize, usize, f32)>;
            let mut matched_headings = std::collections::HashSet::new();
            for line in text_lines(body.collect()) {
                if let Some(first) = line.first() {
                    let right_edge = line
                        .iter()
                        .map(|span| span.bbox.x + span.bbox.width)
                        .fold(first.bbox.x, f32::max);
                    let line_width = right_edge - first.bbox.x;
                    let alignment =
                        visual_alignment(first.bbox.x - left, line_width, width, margin_pt);
                    let indent = left_indent(first.bbox.x, line_width);
                    let font_size = line.iter().map(|span| span.font_size).fold(0.0, f32::max);
                    let line_height = (font_size * 1.2).clamp(1.0, 200.0);
                    let mut paragraph =
                        line_paragraph(&line, &style).align(alignment).line_spacing(
                            LineSpacing::new()
                                .line((line_height * TWIPS_PER_POINT).round() as i32)
                                .line_rule(LineSpacingType::Exact),
                        );
                    if matches!(alignment, AlignmentType::Center)
                        && !line.iter().any(|span| {
                            is_code_font(span)
                                || style
                                    .annotations
                                    .iter()
                                    .any(|annotation| annotation_covers(annotation, &span.bbox))
                        })
                        && let Some((equation, number)) = structure.equation(&line)
                    {
                        let spacing = paragraph.property.line_spacing;
                        paragraph = Paragraph::new().align(alignment).add_run(equation);
                        if !number.is_empty() {
                            paragraph = paragraph.add_run(Run::new().add_text(number));
                        }
                        paragraph.property.line_spacing = spacing;
                    }
                    if matches!(alignment, AlignmentType::Left) {
                        paragraph = paragraph.indent(Some(indent), None, None, None);
                        // 页眉图标下的长横线可直接使用 Word 段落边框。
                        if is_icon(first)
                            && let Some((rule, color)) = paths.iter().find_map(|path| {
                                let color = path.stroke_color?;
                                let rule = path.rendered_bbox();
                                let gap = first.bbox.y - rule.y - rule.height;
                                (rule.width >= width * 0.5
                                    && rule.height <= 1.5
                                    && (0.0..=6.0).contains(&gap)
                                    && (rule.x - first.bbox.x).abs() <= 2.0
                                    && rule.x + rule.width >= right_edge)
                                    .then_some((rule, color))
                            })
                        {
                            let right_indent = ((left + width - margin_pt - rule.x - rule.width)
                                .max(0.0)
                                * TWIPS_PER_POINT)
                                .round() as i32;
                            paragraph =
                                paragraph.indent(Some(indent), None, Some(right_indent), None);
                            paragraph.property = paragraph.property.set_borders(
                                ParagraphBorders::with_empty().set(
                                    ParagraphBorder::new(ParagraphBorderPosition::Bottom)
                                        .size((rule.height * 8.0).round().clamp(2.0, 12.0) as usize)
                                        .color(rgb_hex(pdf_appearance::rgb(color))),
                                ),
                            );
                        }
                    }
                    // 以原 PDF 的填色矩形作为段落底色，同时保留原有行号和代码的起点。
                    let background = paths.iter().rev().find_map(|path| {
                        let color = path.fill_color?;
                        let bounds = path.bbox;
                        (bounds.width >= width * 0.25
                            && bounds.height >= font_size * 0.8
                            && bounds.height <= font_size * 3.0
                            && path.is_rectangle()
                            && (line.iter().any(|span| {
                                is_code_font(span) && bounds.contains_rect(&span.bbox)
                            }) || (line.len() == 1
                                && first.bbox.x + first.bbox.width < bounds.x
                                && (bounds.y..=bounds.y + bounds.height).contains(&first.bbox.y)
                                && code_numbering.is_some_and(|(_, last, y)| {
                                    y - first.bbox.y <= bounds.height * 1.6
                                        && first.text.trim().parse::<usize>().ok()
                                            == last.checked_add(1)
                                }))))
                        .then_some((bounds, color))
                    });
                    let mut code_id = None;
                    if let Some((background, color)) = background {
                        let background_indent = left_indent(background.x, background.width);
                        let right_indent =
                            ((left + width - margin_pt - background.x - background.width).max(0.0)
                                * TWIPS_PER_POINT)
                                .round() as i32;
                        let offset = indent - background_indent;
                        let first_line = if offset < 0 {
                            SpecialIndentType::Hanging(-offset)
                        } else {
                            SpecialIndentType::FirstLine(offset)
                        };
                        paragraph = paragraph.align(AlignmentType::Left).indent(
                            Some(background_indent),
                            Some(first_line),
                            Some(right_indent),
                            None,
                        );
                        paragraph.property = paragraph
                            .property
                            .shading(Shading::new().fill(rgb_hex(pdf_appearance::rgb(color))));
                        if let Ok(number) = first.text.trim().parse::<usize>()
                            && first.bbox.x + first.bbox.width < background.x
                        {
                            let id = match code_numbering {
                                Some((id, previous, y))
                                    if previous.checked_add(1) == Some(number)
                                        && y - first.bbox.y <= background.height * 1.6 =>
                                {
                                    id
                                }
                                _ => {
                                    let id = next_code_id;
                                    next_code_id += 1;
                                    document = document.add_numbering(
                                        Numbering::new(id, 1)
                                            .add_override(LevelOverride::new(0).start(number)),
                                    );
                                    id
                                }
                            };
                            code_numbering = Some((id, number, first.bbox.y));
                            code_id = Some(id);
                            let text_indent = background_indent;
                            let number_end = left_indent(first.bbox.x + first.bbox.width, 0.0);
                            let mut numbered = line_paragraph(&line[1..], &style)
                                .numbering(NumberingId::new(id), IndentLevel::new(0))
                                .indent(
                                    Some(text_indent),
                                    Some(SpecialIndentType::Hanging(
                                        (text_indent - number_end).max(0),
                                    )),
                                    Some(right_indent),
                                    None,
                                )
                                .add_tab(
                                    Tab::new().val(TabValueType::Num).pos(text_indent as usize),
                                );
                            if let Some(start) = line.get(1) {
                                let columns = ((start.bbox.x - background.x)
                                    / (start.font_size * 0.525).max(1.0))
                                .round()
                                .clamp(0.0, 100.0)
                                    as usize;
                                if columns > 0 {
                                    numbered.children.insert(
                                        0,
                                        docx_rs::ParagraphChild::Run(Box::new(
                                            Run::new()
                                                .fonts(
                                                    RunFonts::new()
                                                        .ascii("Consolas")
                                                        .hi_ansi("Consolas"),
                                                )
                                                .size((start.font_size * 2.0).round().max(2.0)
                                                    as usize)
                                                .add_text(" ".repeat(columns)),
                                        )),
                                    );
                                }
                            }
                            numbered.property.line_spacing = paragraph.property.line_spacing;
                            numbered.property.shading = paragraph.property.shading;
                            paragraph = numbered;
                        }
                    }
                    let outline_level = headings.get(&page).and_then(|items| {
                        let key = heading_key(
                            &line
                                .iter()
                                .map(|span| span.text.as_str())
                                .collect::<String>(),
                        );
                        items
                            .get(&key)
                            .copied()
                            .filter(|_| !is_icon(first) && matched_headings.insert(key))
                    });
                    let heading = outline_level
                        .or(first.heading_level.map(usize::from))
                        .or_else(|| {
                            (typical_size.is_some_and(|size| first.font_size > size * 1.35)
                                && line
                                    .iter()
                                    .map(|span| span.text.chars().count())
                                    .sum::<usize>()
                                    < 100)
                                .then_some(1)
                        });
                    if let Some(level) = heading {
                        let level = level.clamp(1, 9);
                        paragraph = paragraph
                            .style(&format!("Heading{level}"))
                            .outline_lvl(level - 1)
                            .keep_next(outline_level.is_none());
                    }
                    let output = background.is_none()
                        && line.iter().all(|span| is_code_font(span))
                        && paths.iter().any(|path| {
                            let rule = path.rendered_bbox();
                            path.stroke_color.is_some()
                                && rule.height <= 1.5
                                && rule.width >= width * 0.25
                                && (rule.x - first.bbox.x).abs() <= 3.0
                                && (rule.y - first.bbox.y).abs() <= 64.0
                        });
                    let block = if background.is_some() || output {
                        WordBlock::Code(Box::new(paragraph), code_id, output)
                    } else {
                        WordBlock::Paragraph(Box::new(paragraph))
                    };
                    blocks.push((
                        first.bbox.y + first.bbox.height,
                        first.bbox.x,
                        first.bbox.y,
                        block,
                    ));
                }
            }
            for (bounds, picture) in vector_images {
                let indent = left_indent(bounds.x, bounds.width);
                let paragraph = image_paragraph(
                    picture,
                    bounds.width,
                    bounds.height,
                    content_width,
                    content_height,
                )
                .indent(Some(indent), None, None, None);
                blocks.push((
                    bounds.y + bounds.height,
                    bounds.x,
                    bounds.y,
                    WordBlock::Paragraph(Box::new(paragraph)),
                ));
            }

            // PNG 可以保留颜色和透明度。遇到无法解码的图片时报告页码。
            for image in images? {
                let bounds = match image
                    .bbox()
                    .copied()
                    .filter(|bounds| bounds.width > 0.0 && bounds.height > 0.0)
                {
                    Some(bounds) => bounds,
                    None => Rect::new(
                        left,
                        bottom,
                        image.width() as f32 * 0.75,
                        image.height() as f32 * 0.75,
                    ),
                };
                let data = match image.to_png_bytes() {
                    Ok(data) => data,
                    Err(error) => {
                        eprintln!("提示：第 {} 页有图片无法解码：{error}", page + 1);
                        continue;
                    }
                };
                let raster = (recognize_pictures
                    && bounds.width >= 90.0
                    && bounds.height >= 30.0
                    && !spans.iter().any(|span| bounds.contains_rect(&span.bbox)))
                .then(|| image::load_from_memory_with_format(&data, image::ImageFormat::Png).ok())
                .flatten();
                let picture = Pic::new_with_dimensions(data, image.width(), image.height());
                let indent = left_indent(bounds.x, bounds.width);
                let mut paragraph = image_paragraph(
                    picture,
                    bounds.width,
                    bounds.height,
                    content_width,
                    content_height,
                )
                .indent(Some(indent), None, None, None);
                // 单独识别位图，原生文字与带文字层的扫描图保持原样。
                if let Some(raster) = raster {
                    if engine.is_none() {
                        engine = ScanOcr::new().ok();
                    }
                    let raster = raster.into_rgb8();
                    if let Some(engine) = &mut engine
                        && let Ok(elements) = engine.recognize(&raster, &[])
                    {
                        let mut found = false;
                        for (_, element) in elements {
                            let lines = match element {
                                ScanElement::Text(lines) => lines,
                                ScanElement::Table { rows, .. } => {
                                    rows.into_iter().flatten().flatten().collect()
                                }
                                ScanElement::Picture => continue,
                            };
                            for line in lines {
                                paragraph = paragraph.add_run(
                                    Run::new()
                                        .add_text(format!(" {}", clean_text(&line.text)))
                                        .vanish(),
                                );
                                found = true;
                            }
                        }
                        if found && report.ocr_pages.last() != Some(&(page + 1)) {
                            report.ocr_pages.push(page + 1);
                        }
                    }
                }
                blocks.push((
                    bounds.y + bounds.height,
                    bounds.x,
                    bounds.y,
                    WordBlock::Paragraph(Box::new(paragraph)),
                ));
            }
        }

        let bookmarks = links.bookmarks.get(&page).map_or(&[][..], Vec::as_slice);
        previous_tables = next_tables;
        document = append_blocks(document, blocks, top, margin_pt, bookmarks);

        // 为每个原始页单独设定纸张尺寸，最后一节的属性放在文档尾部。
        if output_index + 1 < pages.len() {
            let section = SectionProperty::new()
                .page_size(
                    PageSize::new()
                        .size(page_width, page_height)
                        .orient(orientation),
                )
                .page_margin(margin);
            document = document.add_paragraph(
                Paragraph::new()
                    .size(2)
                    .line_spacing(
                        LineSpacing::new()
                            .line(20)
                            .line_rule(LineSpacingType::Exact),
                    )
                    .section_property(section),
            );
        } else {
            document = document
                .page_size(page_width, page_height)
                .page_orient(orientation)
                .page_margin(margin);
        }
    }
    Ok((structure.apply(document.build())?, report))
}
