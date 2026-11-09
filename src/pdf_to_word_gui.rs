//! PDF2Word 的 Windows 桌面入口。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win_gui;

#[cfg(windows)]
fn main() {
    win_gui::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("PDF2Word GUI 需要 Windows。");
}
