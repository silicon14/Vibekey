//! Small helpers around the Win32 API: wide strings, DPI, message boxes and the
//! custom message ids used to talk between the worker threads and the main
//! window.

use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, GetDC, GetDeviceCaps, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, LOGPIXELSX,
    MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics,
    GetWindowThreadProcessId, MessageBoxW, GUITHREADINFO, MB_ICONERROR, MB_ICONINFORMATION,
    MB_ICONWARNING, MB_OK, SM_CXSCREEN, SM_CYSCREEN,
};

/// First message id available to applications.
pub const WM_APP: u32 = 0x8000;

/// wparam = action code, lparam = snippet index for ACTION_SNIPPET.
pub const WM_APP_ACTION: u32 = WM_APP + 1;
/// Tray icon callback message.
pub const WM_APP_TRAY: u32 = WM_APP + 2;
/// Reload the configuration file.
pub const WM_APP_RELOAD: u32 = WM_APP + 3;
/// Ask the sync thread to synchronise now.
pub const WM_APP_SYNC_NOW: u32 = WM_APP + 4;
/// A synchronisation finished; the result is in crate::sync::SyncShared.
pub const WM_APP_SYNC_DONE: u32 = WM_APP + 5;
/// An injection finished; wparam 0 = success, 1 = failure.
pub const WM_APP_INJECT_DONE: u32 = WM_APP + 6;
/// The inject thread handed a clipboard text back for delayed restoration.
pub const WM_APP_CLIP_RESTORE: u32 = WM_APP + 7;
/// The "test connection" worker finished; the result is in the settings state.
pub const WM_APP_TEST_RESULT: u32 = WM_APP + 8;

pub const ACTION_LIST: usize = 1;
pub const ACTION_CONFIG: usize = 2;
pub const ACTION_SNIPPET: usize = 3;

/// WM_TIMER id used to poll the configuration file for modifications.
pub const TIMER_CONFIG_POLL: usize = 1;
/// WM_TIMER id used to restore the clipboard after a paste.
pub const TIMER_CLIP_RESTORE: usize = 2;
/// WM_TIMER id that fires after a single tray click delay.
pub const TIMER_TRAY_CLICK: usize = 3;

/// NUL terminated UTF-16 string.
pub fn wide(text: &str) -> Vec<u16> {
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    buffer.push(0);
    buffer
}

/// Decode a NUL terminated (or shorter) UTF-16 buffer.
pub fn from_wide(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// Convert a HWND to the isize used in atomics and message parameters.
pub fn hwnd_to_isize(hwnd: HWND) -> isize {
    hwnd as isize
}

/// Convert an isize back to a HWND.
pub fn isize_to_hwnd(value: isize) -> HWND {
    value as HWND
}

/// Show a modal message box owned by owner.
pub fn message_box(owner: HWND, text: &str, caption: &str, flags: u32) {
    let text = wide(text);
    let caption = wide(caption);
    // SAFETY: both strings are valid NUL terminated UTF-16 buffers and the
    // window handle is either a live window or null.
    unsafe {
        MessageBoxW(owner, text.as_ptr(), caption.as_ptr(), flags);
    }
}

pub fn message_box_info(owner: HWND, text: &str, caption: &str) {
    message_box(owner, text, caption, MB_OK | MB_ICONINFORMATION);
}

pub fn message_box_warning(owner: HWND, text: &str, caption: &str) {
    message_box(owner, text, caption, MB_OK | MB_ICONWARNING);
}

pub fn message_box_error(owner: HWND, text: &str, caption: &str) {
    message_box(owner, text, caption, MB_OK | MB_ICONERROR);
}

/// DPI scaling factor of the primary monitor (1.0 = 96 DPI). The process is
/// declared system DPI aware with SetProcessDPIAware, so this value is valid for
/// every window of the process.
pub fn dpi_scale() -> f32 {
    static SCALE: OnceLock<f32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        // SAFETY: GetDC(null) returns the screen DC; it is released again below.
        unsafe {
            let hdc = GetDC(std::ptr::null_mut());
            if hdc.is_null() {
                return 1.0;
            }
            let dpi = GetDeviceCaps(hdc, LOGPIXELSX as i32);
            ReleaseDC(std::ptr::null_mut(), hdc);
            if dpi <= 0 {
                1.0
            } else {
                (dpi as f32 / 96.0).clamp(0.5, 4.0)
            }
        }
    })
}

/// Scale a logical pixel value to physical pixels.
pub fn scale(value: i32) -> i32 {
    (value as f32 * dpi_scale()).round() as i32
}

/// Position of the text caret of the foreground window, falling back to the
/// mouse cursor.
pub fn caret_pos() -> POINT {
    // SAFETY: all pointers refer to stack values of the right type and the
    // handles come from the OS.
    unsafe {
        let foreground = GetForegroundWindow();
        if !foreground.is_null() {
            let mut process_id = 0u32;
            let thread_id = GetWindowThreadProcessId(foreground, &mut process_id);
            if thread_id != 0 {
                let mut info: GUITHREADINFO = std::mem::zeroed();
                info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
                if GetGUIThreadInfo(thread_id, &mut info) != 0 && !info.hwndCaret.is_null() {
                    let mut point = POINT {
                        x: info.rcCaret.left,
                        y: info.rcCaret.bottom,
                    };
                    if ClientToScreen(info.hwndCaret, &mut point) != 0 {
                        return point;
                    }
                }
            }
        }
        mouse_pos()
    }
}

/// Current cursor position in screen coordinates.
pub fn mouse_pos() -> POINT {
    // SAFETY: point is a valid POINT for the duration of the call.
    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        point
    }
}

/// Move point so that a window of width x height around it stays on the monitor
/// that contains the point.
pub fn clamp_to_monitor(point: POINT, width: i32, height: i32) -> POINT {
    // SAFETY: the monitor info structure is initialised with its size and the
    // handle comes from the OS.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let (left, top, right, bottom) =
            if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
                (
                    info.rcWork.left,
                    info.rcWork.top,
                    info.rcWork.right,
                    info.rcWork.bottom,
                )
            } else {
                (
                    0,
                    0,
                    GetSystemMetrics(SM_CXSCREEN),
                    GetSystemMetrics(SM_CYSCREEN),
                )
            };
        let margin = scale(4);
        let x = point.x.min(right - width - margin).max(left + margin);
        let y = point.y.min(bottom - height - margin).max(top + margin);
        POINT { x, y }
    }
}

/// Current process id.
pub fn current_process_id() -> u32 {
    std::process::id()
}

/// File name (without directory) of a process id, lower cased.
pub fn process_name(process_id: u32) -> Option<String> {
    // SAFETY: the process handle is closed again in every path; the buffer and
    // length pointer are valid for the duration of the call.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if handle.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .map(|name| name.to_ascii_lowercase())
    }
}

/// A rectangle from left/top/right/bottom.
pub fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

/// Write a string into a fixed size UTF-16 field that may live in a packed
/// (1-byte aligned) structure.
///
/// SAFETY: field must point to count writable u16 entries.
pub unsafe fn write_wide_field(field: *mut u16, count: usize, text: &str) {
    if count == 0 || field.is_null() {
        return;
    }
    let mut index = 0usize;
    for unit in text.encode_utf16().take(count - 1) {
        std::ptr::write_unaligned(field.add(index), unit);
        index += 1;
    }
    while index < count {
        std::ptr::write_unaligned(field.add(index), 0);
        index += 1;
    }
}

/// Store a pointer sized value in the GWLP_USERDATA slot of a window. On 32 bit
/// Windows the Ptr variants of the window long functions do not exist, so the
/// plain variants are used there.
#[cfg(target_pointer_width = "64")]
pub fn set_window_userdata(hwnd: HWND, value: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_USERDATA};
    // SAFETY: the handle is a live window and the index is the documented user
    // data slot.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, value);
    }
}

#[cfg(target_pointer_width = "32")]
pub fn set_window_userdata(hwnd: HWND, value: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongW, GWLP_USERDATA};
    // SAFETY: see the 64 bit variant.
    unsafe {
        SetWindowLongW(hwnd, GWLP_USERDATA, value as i32);
    }
}

/// Read the GWLP_USERDATA slot of a window.
#[cfg(target_pointer_width = "64")]
pub fn window_userdata(hwnd: HWND) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWLP_USERDATA};
    // SAFETY: the handle is a live window and the index is the documented user
    // data slot.
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
}

#[cfg(target_pointer_width = "32")]
pub fn window_userdata(hwnd: HWND) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWLP_USERDATA};
    // SAFETY: see the 64 bit variant.
    unsafe { GetWindowLongW(hwnd, GWLP_USERDATA) as isize }
}

/// Replace the window procedure of a window and return the previous one.
#[cfg(target_pointer_width = "64")]
pub fn replace_window_proc(hwnd: HWND, procedure: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_WNDPROC};
    // SAFETY: the handle is a live window and the new procedure has the
    // WNDPROC signature.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, procedure) }
}

#[cfg(target_pointer_width = "32")]
pub fn replace_window_proc(hwnd: HWND, procedure: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongW, GWLP_WNDPROC};
    // SAFETY: see the 64 bit variant.
    unsafe { SetWindowLongW(hwnd, GWLP_WNDPROC, procedure as i32) as isize }
}
