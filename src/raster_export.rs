//! 读取和导出页面图片，并将图片封装为纯图像 PDF。

use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbImage, RgbaImage,
    codecs::jpeg::{JpegEncoder, PixelDensity},
};
use pdf_oxide::{
    PdfDocument,
    geometry::Rect,
    rendering::{RenderOptions, render_page},
    writer::{DocumentBuilder, PageSize},
};
use tempfile::{NamedTempFile, tempdir_in};

use crate::{
    ConfirmOverwrite, OutputSkipped, PageSelection, has_extension, open_pdf, persist_output,
    selected_pages,
};

/// JPEG 编码器的默认质量。
pub const DEFAULT_JPEG_QUALITY: u8 = 90;
const EXPORT_PIXEL_LIMIT: u64 = 40_000_000;

/// 读取单张 PNG、JPEG 或 BMP，应用方向元数据并把透明像素合成到白底。
pub(crate) fn read_image(path: &Path) -> Result<RgbImage> {
    let reader = ImageReader::open(path)?.with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Bmp)
        ),
        "支持 PNG、JPEG 和 BMP 图片"
    );
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    if image.color().has_alpha() {
        let rgba = image.into_rgba8();
        Ok(RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            let [red, green, blue, alpha] = rgba.get_pixel(x, y).0;
            let alpha = u16::from(alpha);
            image::Rgb(
                [red, green, blue].map(|value| {
                    ((u16::from(value) * alpha + 255 * (255 - alpha) + 127) / 255) as u8
                }),
            )
        }))
    } else {
        Ok(image.into_rgb8())
    }
}

/// 给单张图片设置页面尺寸，最长边为常用纸张长度。
pub(crate) fn image_page_size(image: &RgbImage) -> Result<(f32, f32)> {
    ensure!(
        image.width() > 0 && image.height() > 0,
        "输入图片没有有效像素"
    );
    let scale = 792.0 / image.width().max(image.height()) as f32;
    Ok((image.width() as f32 * scale, image.height() as f32 * scale))
}

/// 编码一张图片供 Word 和 PDF 使用。
pub(crate) fn encode_png(image: &RgbImage) -> Result<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png)?;
    Ok(bytes.into_inner())
}

/// 将选中的 PDF 页逐页渲染，再按原页尺寸写成仅含图片的 PDF。
pub(crate) fn pdf_as_images(pdf: &PdfDocument, pages: &[usize], dpi: u32) -> Result<Vec<u8>> {
    let mut output = DocumentBuilder::new();
    let options = RenderOptions::with_dpi(dpi);
    for &page in pages {
        let (left, bottom, right, top) = pdf.get_page_media_box(page)?;
        let (mut width, mut height) = ((right - left).abs(), (top - bottom).abs());
        if pdf.get_page_rotation(page)?.rem_euclid(180) == 90 {
            std::mem::swap(&mut width, &mut height);
        }
        let image = render_page(pdf, page, &options)
            .with_context(|| format!("无法渲染 PDF 第 {} 页", page + 1))?;
        add_image_page(&mut output, &image.data, width, height)?;
    }
    Ok(output.build()?)
}

/// 将单张图片写成一页 PDF。
pub(crate) fn image_as_pdf(image: &RgbImage) -> Result<Vec<u8>> {
    let (width, height) = image_page_size(image)?;
    let mut output = DocumentBuilder::new();
    add_image_page(&mut output, &encode_png(image)?, width, height)?;
    Ok(output.build()?)
}

fn add_image_page(output: &mut DocumentBuilder, png: &[u8], width: f32, height: f32) -> Result<()> {
    output
        .page(PageSize::Custom(width, height))
        .image_from_bytes(png, Rect::new(0.0, 0.0, width, height))?
        .done();
    Ok(())
}

/// 单页导出到给定 PNG 路径，多页导出到同名的图片目录。
pub fn export_png(input: &Path, output: &Path, dpi: u32) -> Result<PathBuf> {
    ensure!(has_extension(output, "png"), "图片输出扩展名必须为 .png");
    export_image(input, output, dpi)
}

/// 按目标扩展名导出 PNG 或 JPG，并返回实际输出的文件或多页目录。
/// PDF 按指定 DPI 渲染。输入图片保留像素尺寸，并记录导出 DPI。
pub fn export_image(input: &Path, output: &Path, dpi: u32) -> Result<PathBuf> {
    export_image_with_quality(input, output, dpi, DEFAULT_JPEG_QUALITY)
}

/// 导出页面图片，JPEG 质量可设为 1 至 100，PNG 使用无损编码。
pub fn export_image_with_quality(
    input: &Path,
    output: &Path,
    dpi: u32,
    jpeg_quality: u8,
) -> Result<PathBuf> {
    export_selected_images(input, output, dpi, jpeg_quality, None)
}

/// 导出选中的 PDF 页面。多张图片使用原 PDF 页码命名。
/// 直接转换输入图片时忽略页码范围。
pub fn export_selected_images(
    input: &Path,
    output: &Path,
    dpi: u32,
    jpeg_quality: u8,
    selection: Option<&PageSelection>,
) -> Result<PathBuf> {
    export_selected_images_confirming(input, output, dpi, jpeg_quality, selection, None)
}

/// 导出选中页面，并在替换已有图片之前征求确认。
pub fn export_selected_images_confirming(
    input: &Path,
    output: &Path,
    dpi: u32,
    jpeg_quality: u8,
    selection: Option<&PageSelection>,
    confirm: ConfirmOverwrite<'_>,
) -> Result<PathBuf> {
    ensure!(input.is_file(), "输入文件不存在：{}", input.display());
    let jpeg = has_extension(output, "jpg") || has_extension(output, "jpeg");
    ensure!(
        jpeg || has_extension(output, "png"),
        "图片输出扩展名必须为 .png、.jpg 或 .jpeg"
    );
    let dpi = u16::try_from(dpi).context("图片分辨率超出范围")?;
    ensure!(dpi > 0, "图片分辨率必须大于 0");
    let quality = jpeg.then_some(jpeg_quality);
    ensure!(
        quality.is_none_or(|quality| (1..=100).contains(&quality)),
        "JPG 质量必须为 1–100"
    );
    let directory = match output.parent() {
        Some(path) if !path.as_os_str().is_empty() => path,
        _ => Path::new("."),
    };
    ensure!(
        directory.is_dir(),
        "输出目录不存在：{}",
        directory.display()
    );
    if !has_extension(input, "pdf") {
        let image =
            read_image(input).with_context(|| format!("读取图片失败：{}", input.display()))?;
        return save_image(
            output,
            directory,
            &encode_export_image(&image, dpi, quality)?,
            confirm,
        );
    }

    let pdf = open_pdf(input)?;
    let count = pdf.page_count().context("无法读取 PDF 页数")?;
    let pages = selected_pages(selection, count)?;
    if let [page] = pages.as_slice() {
        let data = render_export_image(&pdf, *page, dpi, quality)
            .with_context(|| format!("无法导出 PDF 第 {} 页", page + 1))?;
        return save_image(output, directory, &data, confirm);
    }

    // 先写完所有页面，再发布整个目录。某页渲染失败时不会留下半套图片。
    let mut name = output
        .file_stem()
        .context("图片输出需要文件名")?
        .to_os_string();
    let extension = if jpeg { "jpg" } else { "png" };
    name.push(if jpeg { "_jpg_images" } else { "_images" });
    let folder = directory.join(name);
    ensure!(
        confirm.is_some() || !folder.try_exists()?,
        "输出目录已存在：{}",
        folder.display()
    );
    let staging = tempdir_in(directory).context("无法创建临时图片目录")?;
    let digits = count.to_string().len().max(3);
    for page in pages {
        let page_number = page + 1;
        let data = render_export_image(&pdf, page, dpi, quality)
            .with_context(|| format!("无法导出 PDF 第 {page_number} 页"))?;
        let filename = format!("page_{page_number:0digits$}.{extension}");
        fs::write(staging.path().join(&filename), data)
            .with_context(|| format!("无法写入 {filename}"))?;
    }
    let publish = if folder.try_exists()? {
        Err(std::io::ErrorKind::AlreadyExists.into())
    } else {
        fs::rename(staging.path(), &folder)
    };
    if let Err(error) = publish {
        let Some(ask) = confirm.filter(|_| folder.is_dir()) else {
            return Err(error).with_context(|| format!("无法保存图片目录：{}", folder.display()));
        };
        let metadata = fs::symlink_metadata(&folder)?;
        ensure!(
            metadata.is_dir() && !metadata.is_symlink(),
            "输出必须是普通目录"
        );
        if !ask(&folder) {
            return Err(OutputSkipped.into());
        }
        // 仅替换本次导出的同名图片，目录中其他文件继续保留。
        let entries = fs::read_dir(staging.path())?.collect::<std::io::Result<Vec<_>>>()?;
        for entry in &entries {
            ensure!(
                !folder.join(entry.file_name()).is_dir(),
                "图片路径与目录冲突：{}",
                folder.join(entry.file_name()).display()
            );
        }
        for entry in entries {
            let path = folder.join(entry.file_name());
            fs::rename(entry.path(), &path)
                .with_context(|| format!("无法保存图片：{}", path.display()))?;
        }
    }
    Ok(folder)
}

/// 直接读取 PDF 渲染的像素，避免先编码 PNG 再解码。
fn render_export_image(
    pdf: &PdfDocument,
    page: usize,
    dpi: u16,
    quality: Option<u8>,
) -> Result<Vec<u8>> {
    let mut options = RenderOptions::with_dpi(u32::from(dpi)).as_raw();
    // 让 A4 等常见纸张支持 600 DPI，同时限制大幅面的内存消耗。
    options.max_output_pixels = EXPORT_PIXEL_LIMIT;
    let info = pdf.get_page_info(page)?;
    let media = info.media_box;
    let crop = info.crop_box.and_then(|crop| media.intersection(&crop));
    let bounds = crop.as_ref().unwrap_or(&media);
    let scale = f32::from(dpi) / 72.0;
    let scale = f64::from(scale);
    let pixels =
        (f64::from(bounds.width) * scale).ceil() * (f64::from(bounds.height) * scale).ceil();
    // 渲染器会自动降低过大的页面分辨率。显式报错可以避免写入错误的 DPI。
    ensure!(
        pixels <= options.max_output_pixels as f64,
        "页面在 {dpi} DPI 下像素过多，请选择较低的分辨率"
    );
    let image = render_page(pdf, page, &options)?;
    let rgba = RgbaImage::from_raw(image.width, image.height, image.data)
        .context("PDF 渲染返回了无效的像素数据")?;
    let rgb = DynamicImage::ImageRgba8(rgba).into_rgb8();
    encode_export_image(&rgb, dpi, quality)
}

/// 写入 PNG 的 pHYs 或 JPEG 的 JFIF 分辨率字段。
fn encode_export_image(image: &RgbImage, dpi: u16, quality: Option<u8>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(quality) = quality {
        let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
        encoder.set_pixel_density(PixelDensity::dpi(dpi));
        encoder.encode_image(image)?;
    } else {
        let pixels_per_meter = (u32::from(dpi) * 10_000 + 127) / 254;
        let mut encoder = png::Encoder::new(&mut bytes, image.width(), image.height());
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_pixel_dims(Some(png::PixelDimensions {
            xppu: pixels_per_meter,
            yppu: pixels_per_meter,
            unit: png::Unit::Meter,
        }));
        let mut writer = encoder.write_header()?;
        writer.write_image_data(image.as_raw())?;
        writer.finish()?;
    }
    Ok(bytes)
}

fn save_image(
    output: &Path,
    directory: &Path,
    data: &[u8],
    confirm: ConfirmOverwrite<'_>,
) -> Result<PathBuf> {
    let mut temporary = NamedTempFile::new_in(directory).context("无法创建临时图片文件")?;
    temporary.write_all(data).context("写入图片失败")?;
    persist_output(temporary, output, confirm)
        .with_context(|| format!("无法保存图片：{}", output.display()))?;
    Ok(output.to_path_buf())
}
