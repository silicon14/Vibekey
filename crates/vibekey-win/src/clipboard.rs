//! Clipboard text helpers.
//!
//! Only CF_UNICODETEXT is touched: the clipboard is opened with retries because
//! another application may hold it for a few milliseconds.

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

/// CF_UNICODETEXT; defined here because the windows-sys module that owns the
/// clipboard format constants (Win32_System_Ole) is not enabled.
pub const CF_UNICODETEXT: u32 = 13;

const OPEN_RETRIES: u32 = 10;
const OPEN_RETRY_DELAY_MS: u64 = 10;

/// Open the clipboard, retrying a few times when another application owns it.
fn open_clipboard(owner: HWND) -> bool {
    for _ in 0..OPEN_RETRIES {
        // SAFETY: the window handle is either null or a live window of this
        // process, which is required by the API.
        if unsafe { OpenClipboard(owner) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(OPEN_RETRY_DELAY_MS));
    }
    false
}

/// Read the current clipboard text, if any.
pub fn read_text() -> Option<String> {
    if !open_clipboard(std::ptr::null_mut()) {
        return None;
    }
    let mut result = None;
    // SAFETY: the clipboard is open and is closed again below; the global
    // memory handle is owned by the clipboard until it is closed.
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT);
        if !handle.is_null() {
            let pointer = GlobalLock(handle as HANDLE);
            if !pointer.is_null() {
                let bytes = GlobalSize(handle as HANDLE);
                let units = std::slice::from_raw_parts(pointer as *const u16, bytes / 2);
                let end = units
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(units.len());
                result = Some(String::from_utf16_lossy(&units[..end]));
                GlobalUnlock(handle as HANDLE);
            }
        }
        CloseClipboard();
    }
    result
}

/// Replace the clipboard text. Returns false when the clipboard could not be
/// written (it stays unchanged in that case).
pub fn write_text(text: &str) -> bool {
    if !open_clipboard(std::ptr::null_mut()) {
        return false;
    }
    let mut ok = false;
    // SAFETY: every global memory block is either handed to the clipboard or
    // freed before this block ends, and the clipboard is closed below.
    unsafe {
        if EmptyClipboard() != 0 {
            let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = units.len() * std::mem::size_of::<u16>();
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !memory.is_null() {
                let pointer = GlobalLock(memory);
                if !pointer.is_null() {
                    std::ptr::copy_nonoverlapping(
                        units.as_ptr() as *const u8,
                        pointer as *mut u8,
                        bytes,
                    );
                    GlobalUnlock(memory);
                    let handle = SetClipboardData(CF_UNICODETEXT, memory as HANDLE);
                    if handle.is_null() {
                        GlobalFree(memory);
                    } else {
                        ok = true;
                    }
                } else {
                    GlobalFree(memory);
                }
            }
        }
        CloseClipboard();
    }
    ok
}
