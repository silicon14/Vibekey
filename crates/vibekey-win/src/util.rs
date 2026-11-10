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
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetActiveWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetClassNameW, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo,
    GetSystemMetrics, GetWindowLongW, GetWindowThreadProcessId, MessageBoxW, SetForegroundWindow,
    SetLayeredWindowAttributes, SetWindowLongW, GUITHREADINFO, GWL_EXSTYLE, LWA_ALPHA,
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, SM_CXSCREEN, SM_CYSCREEN,
    WS_EX_LAYERED,
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
/// The "test connection" worker finished; the result is in the settings state.
pub const WM_APP_TEST_RESULT: u32 = WM_APP + 7;
/// Select the snippet page and open an entry for quick editing.
/// wparam = Config snippet index + 1; zero creates a new snippet.
pub const WM_APP_SHOW_SNIPPET: u32 = WM_APP + 8;
/// Open the queued native menu after the caller's App borrow has ended.
pub const WM_APP_SHOW_MENU: u32 = WM_APP + 9;

pub const ACTION_LIST: usize = 1;
pub const ACTION_CONFIG: usize = 2;
pub const ACTION_SNIPPET: usize = 3;
pub const ACTION_HISTORY: usize = 4;

/// WM_TIMER id used to poll the configuration file for modifications.
pub const TIMER_CONFIG_POLL: usize = 1;
/// WM_TIMER id that fires after a single tray click delay.
pub const TIMER_TRAY_CLICK: usize = 2;

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

/// Apply uniform alpha to a layered top-level window. If the platform rejects
/// it, remove WS_EX_LAYERED so the window falls back to normal opaque drawing.
/// SetLayeredWindowAttributes and the 32-bit style APIs exist on Windows 7 and
/// work for both 32- and 64-bit processes (no pointer-sized value is involved).
pub fn set_window_alpha(hwnd: HWND, alpha: u8) -> bool {
    if hwnd.is_null() {
        return false;
    }
    // SAFETY: hwnd is a live top-level window created with WS_EX_LAYERED;
    // LWA_ALPHA reads only the supplied byte value.
    let applied = unsafe { SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA) } != 0;
    if !applied {
        // SAFETY: extended styles are 32-bit values on both architectures.
        // Preserve every other style bit while removing the unsupported layer.
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
            if style != 0 {
                SetWindowLongW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED as i32));
            }
        }
    }
    applied
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

/// Work area of the monitor nearest to a point, with a primary-screen fallback.
pub fn monitor_work_area(point: POINT) -> RECT {
    // SAFETY: the monitor info structure is initialised with its size and the
    // handle comes from the OS.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
            info.rcWork
        } else {
            rect(
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        }
    }
}

/// Move point so that a window of width x height around it stays on the monitor
/// that contains the point.
pub fn clamp_to_monitor(point: POINT, width: i32, height: i32) -> POINT {
    let work = monitor_work_area(point);
    let margin = scale(4);
    let x = point
        .x
        .min(work.right - width - margin)
        .max(work.left + margin);
    let y = point
        .y
        .min(work.bottom - height - margin)
        .max(work.top + margin);
    POINT { x, y }
}

/// Try to make hwnd the foreground window.
///
/// Windows only lets the foreground process (or a process that received the
/// last user input) change the foreground window. The standard workaround is to
/// attach the input queues of both threads for the duration of the call; it also
/// gives the called window the keyboard focus it needs for the picker and menu.
pub fn force_foreground(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    // SAFETY: all handles are OS handles; the thread input attachment is
    // detached again in the same call, and the pointers are valid stack values.
    unsafe {
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, std::ptr::null_mut())
        };
        let current_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(current_thread, foreground_thread, 1) != 0;
        let mut ok = SetForegroundWindow(hwnd) != 0;
        if !ok {
            // SetActiveWindow/BringWindowToTop succeed in a few cases where
            // SetForegroundWindow is refused, especially while attached.
            SetActiveWindow(hwnd);
            ok = BringWindowToTop(hwnd) != 0;
        }
        if attached {
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
        ok
    }
}

/// Current process id.
pub fn current_process_id() -> u32 {
    std::process::id()
}

/// Window class name of a window, empty when it cannot be queried.
pub fn window_class_name(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return String::new();
    }
    let mut buffer = [0u16; 256];
    // SAFETY: the buffer and its length are valid; GetClassNameW only reads and
    // tolerates stale handles by returning 0.
    let length = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        String::new()
    } else {
        from_wide(&buffer[..length as usize])
    }
}

/// True for the shell surfaces that own the taskbar and the desktop. They may
/// legitimately be the foreground window (after a tray icon click) but they must
/// never be used as an injection target.
pub fn is_shell_surface(hwnd: HWND) -> bool {
    matches!(
        window_class_name(hwnd).as_str(),
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Progman"
            | "WorkerW"
            | "NotifyIconOverflowWindow"
    )
}

/// True when the window belongs to this process.
pub fn is_our_process_window(hwnd: HWND) -> bool {
    let mut process_id = 0u32;
    // SAFETY: process_id is a valid out pointer.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    process_id != 0 && process_id == current_process_id()
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
