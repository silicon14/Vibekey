//! Vibekey: Alt based snippet tool for Windows.
//!
//! The binary is a GUI application without a console. All platform specific code
//! lives below the app, hook, inject, sync and ui modules; the platform
//! independent logic (configuration, hotkey parsing, template expansion and
//! WebDAV synchronisation) is provided by the vibekey-core crate.

#![windows_subsystem = "windows"]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod history;
#[cfg(windows)]
mod hook;
#[cfg(windows)]
mod inject;
#[cfg(windows)]
mod log;
#[cfg(windows)]
mod secret;
#[cfg(windows)]
mod sync;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod util;

#[cfg(windows)]
fn main() {
    let exit_code = app::run();
    std::process::exit(exit_code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vibekey is a Windows only application; build it for Windows");
}
