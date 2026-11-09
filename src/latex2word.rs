//! LaTeX 到 Word 的转换引擎。
//!
//! PDF2Word 与 LaTeX2Word 共用 Pandoc 作为 LaTeX 解析器。优先使用程序目录
//! 中的 pandoc.exe，其次使用 PATH 中的 Pandoc。

use std::{fs, path::{Path, PathBuf}, process::{Command, Stdio}};

use anyhow::{Context, Result, bail, ensure};
use tempfile::TempDir;

#[cfg(feature = "embed-pandoc")]
static EMBEDDED_PANDOC: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/pandoc.exe"));

pub fn convert(input: &Path, output: &Path, replace_existing: bool) -> Result<()> {
    ensure!(input.is_file(), "找不到 LaTeX 源文件：{}", input.display());
    let from = input.extension().and_then(|e| e.to_str()).and_then(|e| match e.to_ascii_lowercase().as_str() { "tex"|"latex"=>Some("latex"),"md"|"markdown"=>Some("markdown"),"html"|"htm"=>Some("html"),"txt"=>Some("markdown"),"rst"=>Some("rst"),"odt"=>Some("odt"),"rtf"=>Some("rtf"),"epub"=>Some("epub"),"json"=>Some("json"),_=>None }).context("不支持的 Pandoc 输入格式")?;
    ensure!(output.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("docx")), "LaTeX 输出文件应使用 .docx 后缀");
    if output.exists() && !replace_existing {
        bail!("输出文件已存在：{}", output.display());
    }
    let output = std::path::absolute(output)?;
    let input = std::path::absolute(input)?;
    let output_dir = output.parent().context("无法确定输出目录")?;
    fs::create_dir_all(output_dir)?;
    let stage = TempDir::new_in(output_dir)?;
    let staged = stage.path().join(output.file_name().context("输出文件名无效")?);
    let (executable, _pandoc_dir) = prepare_pandoc()?;
    let mut command = Command::new(executable);
    command.current_dir(input.parent().context("无法确定源文件目录")?)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
        .args(["--standalone", "--citeproc", "--from", from, "--to", "docx", "--output"])
        .arg(&staged).arg("--").arg(input.file_name().context("源文件名无效")?);
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let result = command.output().context("无法启动 Pandoc，请安装 Pandoc 或将 pandoc.exe 放到 PDF2Word 旁")?;
    if !result.status.success() {
        let diagnostic = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        bail!("Pandoc 转换失败：{}", if diagnostic.is_empty() { result.status.to_string() } else { diagnostic });
    }
    ensure!(staged.is_file(), "Pandoc 未生成 DOCX 文件");
    if replace_existing { fs::rename(&staged, &output)?; }
    else { tempfile::TempPath::try_from_path(staged)?.persist_noclobber(&output).map_err(|error| error.error)?; }
    Ok(())
}

pub fn supported_formats() -> &'static [&'static str] {
    &["tex", "latex", "md", "markdown", "html", "htm", "txt", "rst", "odt", "rtf", "epub", "json"]
}

fn prepare_pandoc() -> Result<(PathBuf, Option<TempDir>)> {
    let alongside = std::env::current_exe().context("无法确定程序所在目录")?.with_file_name(format!("pandoc{}", std::env::consts::EXE_SUFFIX));
    if alongside.is_file() {
        return Ok((alongside, None));
    }
    #[cfg(not(feature = "embed-pandoc"))]
    return Ok((PathBuf::from(format!("pandoc{}", std::env::consts::EXE_SUFFIX)), None));
    #[cfg(feature = "embed-pandoc")]
    {
    let directory = tempfile::Builder::new().prefix("pdf2word-pandoc-").tempdir()?;
    hide_directory(directory.path())?;
    let path = directory.path().join(format!("pandoc{}", std::env::consts::EXE_SUFFIX));
    fs::write(&path, EMBEDDED_PANDOC).context("无法解压内嵌 Pandoc")?;
    Ok((path, Some(directory)))
    }
}

#[cfg(all(windows, feature = "embed-pandoc"))]
fn hide_directory(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    ensure!(unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) } != 0,
        "无法隐藏 Pandoc 临时目录：{}", path.display());
    Ok(())
}

#[cfg(any(not(windows), not(feature = "embed-pandoc")))]
fn hide_directory(_path: &Path) -> Result<()> {
    Ok(())
}
