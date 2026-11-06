//! Minimal logger writing to %APPDATA%\Vibekey\vibekey.log.
//!
//! The file is rotated to vibekey.log.1 once it grows beyond one megabyte. When
//! the file cannot be opened the messages are sent to the debugger with
//! OutputDebugStringW instead, so a broken installation still leaves a trace.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;

use crate::util::wide;

const MAX_BYTES: u64 = 1024 * 1024;

struct Logger {
    state: Mutex<State>,
}

struct State {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Logger {
    fn write_line(&self, line: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.file.is_none() {
            state.open();
        }
        let wrote = match state.file.as_mut() {
            Some(file) => {
                let wrote = file.write_all(line.as_bytes()).is_ok();
                let _ = file.flush();
                wrote
            }
            None => false,
        };
        if wrote {
            state.size += line.len() as u64;
            if state.size >= MAX_BYTES {
                state.rotate();
            }
        } else if state.file.is_none() {
            let text = wide(line);
            // SAFETY: the string is a valid NUL terminated UTF-16 buffer.
            unsafe {
                OutputDebugStringW(text.as_ptr());
            }
        }
    }
}

impl State {
    fn open(&mut self) {
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.size = fs::metadata(&self.path).map(|meta| meta.len()).unwrap_or(0);
    }

    fn rotate(&mut self) {
        self.file = None;
        let backup = self.path.with_extension("log.1");
        let _ = fs::remove_file(&backup);
        let _ = fs::rename(&self.path, &backup);
        self.size = 0;
        self.open();
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level(),
            record.target(),
            record.args()
        );
        self.write_line(&line);
    }

    fn flush(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(file) = state.file.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// Initialise the global logger. Safe to call more than once; only the first
/// call installs the logger.
pub fn init(dir: &Path) {
    let level = match std::env::var("VIBEKEY_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" => log::LevelFilter::Off,
        "error" => log::LevelFilter::Error,
        "warn" | "warning" => log::LevelFilter::Warn,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    };

    let _ = fs::create_dir_all(dir);
    let path = dir.join("vibekey.log");
    let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let logger = Logger {
        state: Mutex::new(State {
            path,
            file: None,
            size,
        }),
    };
    // The logger lives for the whole process; leaking the box keeps the
    // 'static reference required by log::set_logger without enabling the
    // optional alloc feature of the log crate.
    let logger: &'static Logger = Box::leak(Box::new(logger));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
    log::info!("vibekey {} starting", env!("CARGO_PKG_VERSION"));
}

/// Path of the log file inside the configuration directory.
pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("vibekey.log")
}
