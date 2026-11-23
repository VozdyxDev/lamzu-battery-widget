#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod card;
#[cfg(windows)]
mod composition;
#[cfg(windows)]
mod wallpaper;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Виджет поддерживает Windows 10 и 11.");
}
