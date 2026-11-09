//! PDF2Word 转换引擎，供命令行和图形界面共同使用。

use std::{
    ffi::OsStr,
    fmt,
    io::{ErrorKind, Write},
    ops::RangeInclusive,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, ensure};
use pdf_oxide::PdfDocument;
use tempfile::NamedTempFile;

mod page_conversion;
mod pdf_appearance;
mod pdf_links;
mod raster_export;
mod scan_ocr;
mod scan_style;
mod word_spacing;
mod word_structure;
pub mod latex2word;

pub use page_conversion::{PageReport, PdfMode};
pub use raster_export::{
    DEFAULT_JPEG_QUALITY, export_image, export_image_with_quality, export_png,
    export_selected_images, export_selected_images_confirming,
};

/// 用户选择保留已有输出。
#[derive(Debug)]
pub struct OutputSkipped;

impl fmt::Display for OutputSkipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("已保留原有输出")
    }
}

impl std::error::Error for OutputSkipped {}

type ConfirmOverwrite<'a> = Option<&'a mut dyn FnMut(&Path) -> bool>;

/// 发布已经写完的文件。确认前保留现有内容。
fn persist_output(staged: NamedTempFile, path: &Path, confirm: ConfirmOverwrite<'_>) -> Result<()> {
    match staged.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists && confirm.is_some() => {
            if confirm.is_some_and(|ask| ask(path)) {
                error.file.persist(path)?;
                Ok(())
            } else {
                Err(OutputSkipped.into())
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// 转换参数。默认优先使用原文字层，并处理所有页。
pub struct ConversionOptions {
    pub pages: Option<PageSelection>,
    pub mode: PdfMode,
    pub dpi: u32,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            pages: None,
            mode: PdfMode::Auto,
            dpi: 150,
        }
    }
}

fn selected_pages(selection: Option<&PageSelection>, page_count: usize) -> Result<Vec<usize>> {
    ensure!(page_count > 0, "源文件中没有页面");
    match selection {
        Some(selection) => selection.indices(page_count),
        None => Ok((0..page_count).collect()),
    }
}

/// 从输入路径生成默认的同名 DOCX 路径。
pub fn default_word_output(input: &Path) -> PathBuf {
    input.with_extension("docx")
}

/// 用户指定的一组页码区间，每个区间均包含起止页。
#[derive(Clone)]
pub struct PageSelection(Vec<RangeInclusive<usize>>);

impl FromStr for PageSelection {
    type Err = anyhow::Error;

    /// 解析用逗号分隔的页码和范围，拒绝空范围、零页及逆序范围。
    fn from_str(value: &str) -> Result<Self> {
        let mut ranges = Vec::new();
        for part in value.split(',') {
            let part = part.trim();
            let (start, end) = match part.split_once('-') {
                Some((start, end)) => (start.trim(), end.trim()),
                None => (part, part),
            };
            let start: usize = start.parse().context("无效的起始页码")?;
            let end: usize = end.parse().context("无效的结束页码")?;
            ensure!(start > 0 && start <= end, "页码范围无效：{part}");
            ranges.push(start..=end);
        }
        Ok(Self(ranges))
    }
}

impl PageSelection {
    /// 按 PDF 页序生成去重后的零基索引，同时检查范围是否超出文档。
    fn indices(&self, page_count: usize) -> Result<Vec<usize>> {
        for range in &self.0 {
            ensure!(
                *range.end() <= page_count,
                "页码 {} 超出文档总页数 {page_count}",
                range.end()
            );
        }
        Ok((0..page_count)
            .filter(|index| self.0.iter().any(|range| range.contains(&(index + 1))))
            .collect())
    }
}

/// 验证路径的扩展名，允许扩展名使用大写或小写。
fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn open_pdf(path: &Path) -> Result<PdfDocument> {
    let pdf =
        PdfDocument::open(path).with_context(|| format!("读取 PDF 失败：{}", path.display()))?;
    if pdf.is_encrypted() {
        ensure!(pdf.authenticate(b"")?, "无法打开加密 PDF，请先解密后再转换");
    }
    Ok(pdf)
}

/// 读取 PDF 总页数，无需渲染页面。
pub fn read_pdf_page_count(path: &Path) -> Result<usize> {
    open_pdf(path)?.page_count().context("无法读取 PDF 页数")
}

/// 读取 PDF 或图片，根据目标扩展名写入 DOCX 或纯图像 PDF。
pub fn convert_document(
    input_path: &Path,
    output_path: &Path,
    options: &ConversionOptions,
) -> Result<PageReport> {
    convert_document_confirming(input_path, output_path, options, None)
}

/// 转换文件，并通过回调确认是否替换同名输出。
pub fn convert_document_confirming(
    input_path: &Path,
    output_path: &Path,
    options: &ConversionOptions,
    confirm: ConfirmOverwrite<'_>,
) -> Result<PageReport> {
    // 先检查参数，避免在路径错误时解析源文件。
    ensure!(
        input_path.is_file(),
        "输入文件不存在：{}",
        input_path.display()
    );
    let raster_pdf = has_extension(output_path, "pdf");
    ensure!(
        raster_pdf || has_extension(output_path, "docx"),
        "输出扩展名必须为 .docx 或 .pdf"
    );
    ensure!(
        confirm.is_some() || !output_path.try_exists()?,
        "输出文件已存在：{}",
        output_path.display()
    );
    let output_dir = match output_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    ensure!(
        output_dir.is_dir(),
        "输出目录不存在：{}",
        output_dir.display()
    );

    // 在目标目录先写入临时文件，保存成功后再提交，并防止覆盖已有文件。
    let mut staged_file =
        NamedTempFile::new_in(output_dir).context("无法在目标目录创建临时文件")?;

    let report = if has_extension(input_path, "pdf") {
        let pdf = open_pdf(input_path)?;
        let pages = selected_pages(
            options.pages.as_ref(),
            pdf.page_count().context("无法读取 PDF 页数")?,
        )?;
        if raster_pdf {
            staged_file
                .write_all(&raster_export::pdf_as_images(&pdf, &pages, options.dpi)?)
                .context("写入图像 PDF 失败")?;
            page_conversion::PageReport::default()
        } else {
            let (document, report) =
                page_conversion::build_document(&pdf, &pages, options.mode, options.dpi)?;
            document
                .pack(staged_file.as_file_mut())
                .context("写入 DOCX 失败")?;
            report
        }
    } else {
        selected_pages(options.pages.as_ref(), 1)?;
        let image = raster_export::read_image(input_path)
            .with_context(|| format!("读取图片失败：{}", input_path.display()))?;
        if raster_pdf {
            staged_file
                .write_all(&raster_export::image_as_pdf(&image)?)
                .context("写入图像 PDF 失败")?;
            page_conversion::PageReport::default()
        } else {
            let (document, report) = page_conversion::build_image_document(&image, options.mode)?;
            document
                .build()
                .pack(staged_file.as_file_mut())
                .context("写入 DOCX 失败")?;
            report
        }
    };

    staged_file
        .as_file()
        .sync_all()
        .context("刷新输出文件失败")?;
    persist_output(staged_file, output_path, confirm)
        .with_context(|| format!("无法保存输出文件：{}", output_path.display()))?;

    Ok(report)
}
