//! Shared visual primitives for Vibekey's native Win32 interface.
//!
//! The application deliberately does not use a GUI framework.  This module
//! centralises the small set of colours, fonts and drawing helpers used by the
//! settings window and snippet picker so both surfaces feel like one product.
//! All APIs and fonts used here are available on Windows 7.

use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{COLORREF, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetStockObject, RoundRect,
    SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DEFAULT_PITCH,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS,
    FW_NORMAL, FW_SEMIBOLD, HBRUSH, HDC, HFONT, NULL_PEN, TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_SETFONT};

use crate::util::{scale, wide};

pub const ACCENT: COLORREF = rgb(15, 108, 189);
pub const ACCENT_DARK: COLORREF = rgb(17, 94, 163);
pub const ACCENT_SOFT: COLORREF = rgb(232, 241, 251);
pub const BACKGROUND: COLORREF = rgb(245, 246, 248);
pub const SURFACE: COLORREF = rgb(255, 255, 255);
pub const SIDEBAR: COLORREF = rgb(247, 247, 247);
pub const SIDEBAR_HOVER: COLORREF = rgb(238, 238, 238);
pub const TEXT: COLORREF = rgb(27, 27, 27);
pub const TEXT_MUTED: COLORREF = rgb(97, 97, 97);
pub const BORDER: COLORREF = rgb(225, 225, 225);

/// Construct a Win32 COLORREF (0x00BBGGRR) from familiar RGB components.
pub const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

struct Objects {
    body: usize,
    small: usize,
    semibold: usize,
    title: usize,
    mono: usize,
    background: usize,
    surface: usize,
}

static OBJECTS: OnceLock<Objects> = OnceLock::new();

fn objects() -> &'static Objects {
    OBJECTS.get_or_init(|| {
        // SAFETY: font creation only reads the static face-name buffer.  The
        // returned GDI objects intentionally live for the process lifetime,
        // just like stock objects, and are shared by all UI windows.
        unsafe {
            Objects {
                body: create_font(14, FW_NORMAL, "Microsoft YaHei UI") as usize,
                small: create_font(12, FW_NORMAL, "Microsoft YaHei UI") as usize,
                semibold: create_font(14, FW_SEMIBOLD, "Microsoft YaHei UI") as usize,
                title: create_font(20, FW_SEMIBOLD, "Microsoft YaHei UI") as usize,
                mono: create_font(13, FW_NORMAL, "Consolas") as usize,
                background: CreateSolidBrush(BACKGROUND) as usize,
                surface: CreateSolidBrush(SURFACE) as usize,
            }
        }
    })
}

unsafe fn create_font(height: i32, weight: u32, face: &str) -> HFONT {
    let face = wide(face);
    CreateFontW(
        -scale(height),
        0,
        0,
        0,
        weight as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        0,
        0,
        CLEARTYPE_QUALITY as u32,
        DEFAULT_PITCH as u32 | FF_SWISS as u32,
        face.as_ptr(),
    )
}

pub fn body_font() -> HFONT {
    objects().body as HFONT
}

pub fn small_font() -> HFONT {
    objects().small as HFONT
}

pub fn semibold_font() -> HFONT {
    objects().semibold as HFONT
}

pub fn title_font() -> HFONT {
    objects().title as HFONT
}

pub fn mono_font() -> HFONT {
    objects().mono as HFONT
}

pub fn background_brush() -> HBRUSH {
    objects().background as HBRUSH
}

pub fn surface_brush() -> HBRUSH {
    objects().surface as HBRUSH
}

/// Apply one of the shared fonts to a native control.
pub fn set_font(hwnd: HWND, font: HFONT) {
    if hwnd.is_null() || font.is_null() {
        return;
    }
    // SAFETY: the handle is a live control and the shared font remains valid
    // for the entire process.  WM_SETFONT does not take ownership of it.
    unsafe {
        SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
    }
}

/// Fill a rectangle with a short-lived solid brush.
pub fn fill(hdc: HDC, rect: &RECT, color: COLORREF) {
    // SAFETY: the DC and rectangle are supplied by the active paint/draw
    // message.  The brush is deleted immediately after use.
    unsafe {
        let brush = CreateSolidBrush(color);
        if brush.is_null() {
            return;
        }
        FillRect(hdc, rect, brush);
        DeleteObject(brush);
    }
}

/// Draw a filled rounded rectangle without an outline.
pub fn fill_rounded(hdc: HDC, rect: &RECT, radius: i32, color: COLORREF) {
    // SAFETY: all objects belong to this paint operation.  Previous selections
    // are restored before the temporary brush is deleted.
    unsafe {
        let brush = CreateSolidBrush(color);
        if brush.is_null() {
            return;
        }
        let null_pen = GetStockObject(NULL_PEN);
        let old_brush = SelectObject(hdc, brush);
        let old_pen = SelectObject(hdc, null_pen);
        RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius,
            radius,
        );
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        DeleteObject(brush);
    }
}

/// Draw a single line of text using one of the shared fonts.
pub fn text(
    hdc: HDC,
    rect: &RECT,
    value: &str,
    font: HFONT,
    color: COLORREF,
    vertically_centred: bool,
) {
    let mut draw_rect = *rect;
    let value = wide(value);
    let mut flags = DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX;
    if vertically_centred {
        flags |= DT_VCENTER;
    }
    // SAFETY: the DC and rectangle belong to the current draw operation, the
    // UTF-16 buffer is NUL terminated, and the shared font outlives the call.
    unsafe {
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, color);
        DrawTextW(hdc, value.as_ptr(), -1, &mut draw_rect, flags);
        SelectObject(hdc, old_font);
    }
}

/// Draw a centred single line, useful for buttons and shortcut badges.
pub fn text_centered(hdc: HDC, rect: &RECT, value: &str, font: HFONT, color: COLORREF) {
    let mut draw_rect = *rect;
    let value = wide(value);
    // SAFETY: see text(); the shared font and UTF-16 buffer outlive the call.
    unsafe {
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, color);
        DrawTextW(
            hdc,
            value.as_ptr(),
            -1,
            &mut draw_rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(hdc, old_font);
    }
}

/// Draw multi-line text, truncating with an ellipsis if required.
pub fn text_multiline(hdc: HDC, rect: &RECT, value: &str, font: HFONT, color: COLORREF) {
    let mut draw_rect = *rect;
    let value = wide(value);
    // SAFETY: see text(); DT_WORDBREAK allows normal wrapped prose.
    unsafe {
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, color);
        DrawTextW(
            hdc,
            value.as_ptr(),
            -1,
            &mut draw_rect,
            DT_LEFT | DT_NOPREFIX | windows_sys::Win32::Graphics::Gdi::DT_WORDBREAK,
        );
        SelectObject(hdc, old_font);
    }
}
