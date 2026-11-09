//! 给 Windows GUI 嵌入应用图标、系统控件主题和 DPI 声明。

use std::{env, io, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=pdf2word_gui.manifest");
    println!("cargo:rerun-if-changed=pdf2word_gui.rc");
    println!("cargo:rerun-if-changed=pdf2word.ico");
    println!("cargo:rerun-if-changed=vendor/pandoc.exe");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = source.join("pdf2word_gui.manifest");
        println!("cargo:rustc-link-arg-bin=pdf_to_word_gui=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=pdf_to_word_gui=/MANIFESTINPUT:{}",
            manifest.display()
        );

        let resource =
            Path::new(&env::var_os("OUT_DIR").ok_or("缺少 OUT_DIR")?).join("pdf2word_gui.res");
        let status = find_msvc_tools::find(&env::var("TARGET")?, "rc.exe")
            .ok_or("找不到 Windows SDK 资源编译器 rc.exe")?
            .current_dir(source)
            .arg("/nologo")
            .arg("/fo")
            .arg(&resource)
            .arg("pdf2word_gui.rc")
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!("编译应用图标失败：{status}")).into());
        }
        println!(
            "cargo:rustc-link-arg-bin=pdf_to_word_gui={}",
            resource.display()
        );
    }
    Ok(())
}
