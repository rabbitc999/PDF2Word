//! PDF2Word 命令行入口。

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use pdf_to_word::{
    ConversionOptions, PageSelection, PdfMode, convert_document, default_word_output,
};

/// 命令行参数以及导出选项。
#[derive(Parser)]
#[command(
    name = "PDF2Word",
    bin_name = "PDF2Word",
    version,
    about = "将 PDF 或图片导出为可编辑 Word 或纯图像 PDF",
    after_long_help = include_str!("../models/english_word_frequency_license.txt")
)]
struct Arguments {
    /// 输入 PDF、PNG、JPEG 或 BMP 路径。
    #[arg(value_name = "INPUT")]
    input_path: PathBuf,

    /// 列出内嵌 Pandoc 支持的输入格式。
    #[arg(long)]
    list_formats: bool,

    /// 输出 .docx 或 .pdf 路径。省略时在输入目录生成同名 .docx。
    #[arg(value_name = "OUTPUT")]
    output_path: Option<PathBuf>,

    /// 仅导出指定页码或范围，例如 1-3,5。页码从 1 开始。
    #[arg(long, value_name = "RANGES")]
    pages: Option<PageSelection>,

    /// 生成 Word 时直接嵌入整页图片。
    #[arg(long, conflicts_with = "ocr")]
    page_images: bool,

    /// PDF 渲染分辨率。Word 的 OCR 模式至少使用 200 DPI。
    #[arg(long, default_value_t = 150, value_parser = clap::value_parser!(u32).range(72..=300))]
    dpi: u32,

    /// 关闭 OCR，优先使用 PDF 原有文字层，扫描页和图片按图片保存。
    #[arg(long, conflicts_with = "ocr")]
    no_ocr: bool,

    /// 强制逐页 OCR。默认优先使用 PDF 原有文字层。
    #[arg(long)]
    ocr: bool,
}

impl Arguments {
    fn mode(&self) -> PdfMode {
        if self.page_images {
            PdfMode::Images
        } else if self.no_ocr {
            PdfMode::Native
        } else if self.ocr {
            PdfMode::Ocr
        } else {
            PdfMode::Auto
        }
    }
}

/// 解析命令行参数，执行转换，并返回可供脚本判断的退出码。
fn main() -> ExitCode {
    // 用 PathBuf 接收系统原生路径，支持中文和空格。
    let arguments = Arguments::parse();

    if arguments.list_formats {
        println!("Pandoc 输入格式：{}", pdf_to_word::latex2word::supported_formats().join(", "));
        return ExitCode::SUCCESS;
    }

    let options = ConversionOptions {
        mode: arguments.mode(),
        dpi: arguments.dpi,
        pages: arguments.pages,
    };
    let output_path = match arguments.output_path {
        Some(path) => path,
        None => default_word_output(&arguments.input_path),
    };

    if arguments
        .input_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tex") || ext.eq_ignore_ascii_case("latex"))
    {
        return match pdf_to_word::latex2word::convert(&arguments.input_path, &output_path, false) {
            Ok(()) => {
                println!("转换完成：{}", output_path.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("转换失败：{error:#}");
                ExitCode::FAILURE
            }
        };
    }

    // 输出结果和以整页图片保存的页码，方便调用方检查转换状态。
    match convert_document(&arguments.input_path, &output_path, &options) {
        Ok(report) => {
            if !report.ocr_pages.is_empty() {
                eprintln!("已进行中英文 OCR 的页码：{:?}", report.ocr_pages);
            }
            if !report.rasterized_pages.is_empty() {
                eprintln!(
                    "提示：以下页面已作为整页图片保存：{:?}",
                    report.rasterized_pages
                );
            }
            println!("转换完成：{}", output_path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("转换失败：{error:#}");
            ExitCode::FAILURE
        }
    }
}
