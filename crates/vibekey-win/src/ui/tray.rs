//! Tray icon handling.
//!
//! The icon is embedded in the executable (assets/icon.ico) and turned into an
//! HICON with CreateIconFromResourceEx, which understands the BMP entries of an
//! ICO file. Fallback is the system application icon.

use windows_sys::Win32::Foundation::{HINSTANCE, HWND};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DestroyIcon, LoadIconW, HICON, IDI_APPLICATION, LR_DEFAULTCOLOR,
};

use crate::util::{write_wide_field, WM_APP_TRAY};

const TRAY_ID: u32 = 1;
const TOOLTIP: &str = "Vibekey · Alt+` 片段 · Alt+V 历史 · 双击管理";
const INFO_TITLE: &str = "Vibekey";
const BALLOON_TIMEOUT_MS: u32 = 8000;

const ICON_BYTES: &[u8] = include_bytes!("../../assets/icon.ico");

/// The tray icon of the main window.
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    own_icon: bool,
    added: bool,
}

impl Tray {
    /// Add the tray icon. Never fails visibly: a missing icon only means that
    /// the fallback icon is used.
    pub fn add(hwnd: HWND) -> Self {
        let (icon, own_icon) = load_icon();
        let mut tray = Tray {
            hwnd,
            icon,
            own_icon,
            added: false,
        };
        tray.re_add();
        tray
    }

    /// (Re-)create the tray icon; used after Explorer restarted.
    pub fn re_add(&mut self) {
        let mut data = self.base_data(NIF_ICON | NIF_MESSAGE | NIF_TIP);
        // SAFETY: szTip is a fixed size array inside the structure; the write
        // uses unaligned stores because NOTIFYICONDATAW is packed on 32 bit
        // Windows.
        unsafe {
            write_wide_field(std::ptr::addr_of_mut!(data.szTip) as *mut u16, 128, TOOLTIP);
        }
        // SAFETY: data is fully initialised and lives for the duration of the
        // call; the window handle is a live window of this process.
        let mut added = unsafe { Shell_NotifyIconW(NIM_ADD, &mut data) } != 0;
        if !added {
            // The shell may still have a stale icon from a previous run.
            // SAFETY: same as above.
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &mut data);
                added = Shell_NotifyIconW(NIM_ADD, &mut data) != 0;
            }
        }
        if !added {
            log::warn!("could not add the tray icon");
        }
        // Keep the flag in sync on both paths: it decides whether the icon is
        // removed on exit and whether balloon tips are shown at all.
        self.added = added;
    }

    /// Show a balloon tip. warning selects the warning icon.
    pub fn notify(&self, text: &str, warning: bool) {
        if !self.added {
            return;
        }
        let mut data = self.base_data(NIF_INFO);
        // SAFETY: szInfo and szInfoTitle are fixed size arrays inside the
        // structure; unaligned stores are required because the structure is
        // packed on 32 bit Windows.
        unsafe {
            write_wide_field(std::ptr::addr_of_mut!(data.szInfo) as *mut u16, 256, text);
            write_wide_field(
                std::ptr::addr_of_mut!(data.szInfoTitle) as *mut u16,
                64,
                INFO_TITLE,
            );
        }
        data.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        data.Anonymous.uTimeout = BALLOON_TIMEOUT_MS;
        // SAFETY: data is fully initialised, the strings are NUL terminated and
        // the window handle is a live window of this process.
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &mut data);
        }
    }

    pub fn remove(&mut self) {
        if !self.added {
            return;
        }
        let mut data = self.base_data(0);
        // SAFETY: data is a valid, initialised NOTIFYICONDATAW.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &mut data);
        }
        self.added = false;
    }

    fn base_data(&self, flags: u32) -> NOTIFYICONDATAW {
        // SAFETY: NOTIFYICONDATAW is a plain old data structure; all zero is a
        // valid starting point that is filled in below.
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = TRAY_ID;
        data.uFlags = flags;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.icon;
        data
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        if self.own_icon && !self.icon.is_null() {
            // SAFETY: the icon was created by CreateIconFromResourceEx and is
            // destroyed exactly once.
            unsafe {
                DestroyIcon(self.icon);
            }
        }
    }
}

/// Load the embedded icon, falling back to the system application icon.
fn load_icon() -> (HICON, bool) {
    if let Some(icon) = icon_from_ico(ICON_BYTES) {
        return (icon, true);
    }
    // SAFETY: the instance may be null for a predefined icon.
    let fallback = unsafe {
        LoadIconW(
            std::ptr::null_mut::<core::ffi::c_void>() as HINSTANCE,
            IDI_APPLICATION,
        )
    };
    (fallback, false)
}

/// Parse the ICO directory and create a 32x32 icon from its BMP entry.
fn icon_from_ico(data: &[u8]) -> Option<HICON> {
    if data.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    let mut entries: Vec<(usize, u32, u32)> = Vec::new();
    for index in 0..count {
        let base = 6 + index * 16;
        if base + 16 > data.len() {
            break;
        }
        let width = data[base] as usize;
        let height = data[base + 1] as usize;
        let size = u32::from_le_bytes([
            data[base + 8],
            data[base + 9],
            data[base + 10],
            data[base + 11],
        ]);
        let offset = u32::from_le_bytes([
            data[base + 12],
            data[base + 13],
            data[base + 14],
            data[base + 15],
        ]);
        let end = offset as u64 + size as u64;
        if end > data.len() as u64 {
            continue;
        }
        let logical = if width == 0 { 256 } else { width };
        if logical != height && height != 0 {
            continue;
        }
        entries.push((offset as usize, size, logical as u32));
    }
    // Prefer exactly 32x32 (the tray icon size), then the smallest larger one.
    entries.sort_by_key(|(_, _, logical)| {
        if *logical == 32 {
            0
        } else if *logical > 32 {
            1 + (*logical - 32)
        } else {
            1000 + (32 - *logical)
        }
    });
    for (offset, size, _) in entries {
        // SAFETY: the range was validated against the buffer length above.
        let icon = unsafe {
            CreateIconFromResourceEx(
                data.as_ptr().add(offset),
                size,
                1,
                0x0003_0000,
                0,
                0,
                LR_DEFAULTCOLOR,
            )
        };
        if !icon.is_null() {
            return Some(icon);
        }
    }
    None
}
