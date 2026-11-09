# 📄 PDF2Word

PDF2Word 是一款面向 Windows 的文档转换工具，可将 PDF、图像和常见文本标记格式转换为可编辑的 Word 文档。

## ✨ 主要功能

- PDF 原生文字层保留与 OCR 转换
- PDF、PNG、JPEG、BMP 转换为 DOCX 或图像 PDF
- LaTeX、Markdown、HTML、纯文本、RST、ODT、RTF、EPUB 和 Pandoc JSON 转换为 DOCX
- 支持单文件和批量转换
- 支持 Pandoc 内嵌版本和外部版本
- Windows 原生图形界面与命令行入口

## 🖥️ 运行环境

- Windows 10 / 11
- Rust 1.85 或更高版本
- 使用 LaTeX 等 Pandoc 格式时，需要内嵌 Pandoc，或在 PATH 中提供 `pandoc.exe`

## 🚀 快速使用

### 图形界面

运行 `pdf_to_word_gui.exe`，添加文件后选择输出目录和格式，点击“开始转换”。

### 命令行

```powershell
PDF2Word.exe input.pdf output.docx
PDF2Word.exe document.tex output.docx
```

## 🔨 构建

根目录提供统一构建脚本。

```bash
# 构建内嵌 Pandoc 的 Debug 版本
./build.sh

# 构建内嵌 Pandoc 的 Release 版本
./build.sh --release

# 构建不内嵌 Pandoc 的 Release 版本
./build.sh --release --no-embed
```

内嵌构建会自动下载 Pandoc Windows x86_64 发布包，并将 `pandoc.exe` 编译进程序。非内嵌构建会优先使用程序目录或系统 PATH 中的 Pandoc。

Windows 下也可以运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build.ps1 -Release
```

## 📦 发布说明

发布内嵌版本时只需分发生成的 PDF2Word 可执行文件。发布非内嵌版本时，需要同时提供 `pandoc.exe`，或要求用户自行安装 Pandoc。

## 📁 目录结构

```text
PDF2Word/
├─ src/                 # PDF、OCR、Word 和 Pandoc 转换实现
├─ models/              # OCR 模型
├─ scripts/build.ps1    # Windows 自动下载 Pandoc 并构建
├─ build.sh             # 跨环境构建入口
├─ vendor/              # 本地构建时的 Pandoc 文件
├─ Cargo.toml           # Rust 项目配置
├─ LICENSE              # MIT 许可证
└─ README.md
```

## 📄 开源协议

本项目采用 [MIT License](LICENSE) 开源协议。

Pandoc 及其相关组件遵循各自的上游许可证。发布包含 Pandoc 的版本时，请同时遵守 Pandoc 发布包中的许可证和版权声明。

## 🤝 参与贡献

欢迎提交 Issue 和 Pull Request。提交代码前请至少执行：

```bash
cargo check
```
