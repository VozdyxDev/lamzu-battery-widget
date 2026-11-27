#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod card;
#[cfg(windows)]
mod composition;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod locale;
#[cfg(windows)]
mod registry;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod wallpaper;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The widget supports Windows 10 and 11.");
}
