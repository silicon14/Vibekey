//! Clipboard helpers.
//!
//! Text is read and written as CF_UNICODETEXT. The clipboard is opened with
//! retries because another application may hold it for a few milliseconds.
//! Before a paste the whole format list is snapshotted, so rich content (files,
//! images, HTML, RTF, OLE objects) can be restored afterwards instead of being
//! destroyed.

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HBITMAP, HGDIOBJ};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CopyImage, IMAGE_BITMAP};

/// CF_UNICODETEXT; defined here because the windows-sys module that owns the
/// clipboard format constants (Win32_System_Ole) is not enabled.
pub const CF_UNICODETEXT: u32 = 13;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_PALETTE: u32 = 9;
const CF_ENHMETAFILE: u32 = 14;
const CF_OWNERDISPLAY: u32 = 0x0080;
const CF_DSPBITMAP: u32 = 0x0082;
const CF_DSPMETAFILEPICT: u32 = 0x0083;
const CF_DSPENHMETAFILE: u32 = 0x008E;
const CF_PRIVATEFIRST: u32 = 0x0200;
const CF_PRIVATELAST: u32 = 0x02FF;
const CF_GDIOBJFIRST: u32 = 0x0300;
const CF_GDIOBJLAST: u32 = 0x03FF;

const OPEN_RETRIES: u32 = 10;
const OPEN_RETRY_DELAY_MS: u64 = 10;
/// Upper bound for one clipboard snapshot. Pasting must not allocate unbounded
/// memory just because the clipboard holds a very large payload; bigger data
/// falls back to the (fast) SendInput path and leaves the clipboard alone.
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
/// A defensive cap for erratic clipboard owners.
const MAX_CLIPBOARD_FORMATS: usize = 256;

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

/// Read CF_UNICODETEXT while the clipboard is already open.
///
/// SAFETY: the clipboard must be open and owned by the calling thread.
unsafe fn read_text_locked() -> Option<String> {
    let handle = GetClipboardData(CF_UNICODETEXT);
    if handle.is_null() {
        return None;
    }
    let pointer = GlobalLock(handle as HANDLE);
    if pointer.is_null() {
        return None;
    }
    let bytes = GlobalSize(handle as HANDLE);
    let units = std::slice::from_raw_parts(pointer as *const u16, bytes / 2);
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    let text = String::from_utf16_lossy(&units[..end]);
    GlobalUnlock(handle as HANDLE);
    Some(text)
}

/// Read the current clipboard text, if any.
pub fn read_text() -> Option<String> {
    if !open_clipboard(std::ptr::null_mut()) {
        return None;
    }
    // SAFETY: the clipboard is open and is closed exactly once below.
    let result = unsafe {
        let text = read_text_locked();
        CloseClipboard();
        text
    };
    result
}

/// Non-blocking variant for WM_CLIPBOARDUPDATE. A copy notification normally
/// arrives after the source has closed the clipboard; if another process still
/// owns it, skipping this event is preferable to sleeping in the UI thread.
/// Rich formats may accompany CF_UNICODETEXT: history intentionally keeps only
/// their plain-text representation and never clones the other payloads.
pub fn read_text_for_history() -> Option<String> {
    // SAFETY: a null owner is permitted and no other clipboard call is made if
    // OpenClipboard fails. Success is paired with exactly one CloseClipboard.
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        return None;
    }
    // SAFETY: the clipboard is open on this thread and is closed exactly once.
    unsafe {
        let text = read_text_locked();
        CloseClipboard();
        text
    }
}

/// Result of a clipboard mutation. `sequence` is queried while this thread
/// still owns the open clipboard, so another process cannot slip its own write
/// between SetClipboardData/EmptyClipboard and sequence attribution.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClipboardMutation {
    pub(crate) succeeded: bool,
    pub(crate) sequence: Option<u32>,
}

impl ClipboardMutation {
    fn failed() -> Self {
        Self {
            succeeded: false,
            sequence: None,
        }
    }
}

/// Read the sequence while the clipboard is open. Other processes cannot open
/// and replace it until the paired CloseClipboard has run.
fn owned_sequence() -> Option<u32> {
    // SAFETY: GetClipboardSequenceNumber has no preconditions. Calling it before
    // CloseClipboard is what gives the value its ownership guarantee.
    let sequence = unsafe { GetClipboardSequenceNumber() };
    (sequence != 0).then_some(sequence)
}

/// Replace the clipboard text. A failed write can still have emptied the old
/// clipboard; the returned sequence records that mutation as well.
pub fn write_text(text: &str) -> ClipboardMutation {
    if !open_clipboard(std::ptr::null_mut()) {
        return ClipboardMutation::failed();
    }
    let mut outcome = ClipboardMutation::failed();
    // SAFETY: every global memory block is either handed to the clipboard or
    // freed before this block ends. The clipboard remains open until after all
    // sequence reads, preventing another process from changing its ownership.
    unsafe {
        if EmptyClipboard() != 0 {
            outcome.sequence = owned_sequence();
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
                        outcome.succeeded = true;
                        outcome.sequence = owned_sequence();
                    }
                } else {
                    GlobalFree(memory);
                }
            }
        }
        CloseClipboard();
    }
    outcome
}

/// Clipboard formats that cannot be duplicated for a later restore.
///
/// They either contain GDI/owner-rendered handles whose lifetime cannot be
/// copied safely, or private formats with unknown ownership semantics. Keeping
/// the old behaviour (SendInput) is safer than restoring a broken clipboard.
fn unsupported_format(format: u32) -> bool {
    matches!(
        format,
        CF_METAFILEPICT
            | CF_PALETTE
            | CF_ENHMETAFILE
            | CF_OWNERDISPLAY
            | CF_DSPBITMAP
            | CF_DSPMETAFILEPICT
            | CF_DSPENHMETAFILE
    ) || (CF_PRIVATEFIRST..=CF_PRIVATELAST).contains(&format)
        || (CF_GDIOBJFIRST..=CF_GDIOBJLAST).contains(&format)
}

/// One duplicated clipboard payload.
enum SavedData {
    Bytes(Vec<u8>),
    Bitmap(HBITMAP),
}

struct SavedFormat {
    format: u32,
    data: SavedData,
}

impl Drop for SavedFormat {
    fn drop(&mut self) {
        if let SavedData::Bitmap(handle) = &mut self.data {
            let bitmap = std::mem::replace(handle, std::ptr::null_mut());
            if !bitmap.is_null() {
                // SAFETY: the handle was created by CopyImage in snapshot() and
                // is owned by this snapshot until it is given to the clipboard.
                unsafe {
                    DeleteObject(bitmap as HGDIOBJ);
                }
            }
        }
    }
}

/// A complete copy of the clipboard formats taken before a paste.
///
/// Rich content (files, images, HTML/RTF, OLE objects, ...) is duplicated into
/// private memory so that the snippet can still be pasted with Ctrl+V and the
/// original clipboard can be put back afterwards. Formats that cannot be
/// duplicated safely make `snapshot()` fail; the caller then uses SendInput and
/// leaves the clipboard untouched.
pub struct ClipboardSnapshot {
    formats: Vec<SavedFormat>,
}

impl ClipboardSnapshot {
    /// Put every saved format back on the clipboard, replacing whatever the
    /// paste left there. Returns the usually filtered internal mutation record.
    pub fn restore(&mut self) -> ClipboardMutation {
        if !open_clipboard(std::ptr::null_mut()) {
            return ClipboardMutation::failed();
        }
        let mut outcome = ClipboardMutation::failed();
        // SAFETY: every block allocated below is either handed to the clipboard
        // or freed on the same path. The clipboard is closed exactly once, after
        // the sequence number is read while ownership is still exclusive.
        unsafe {
            if EmptyClipboard() != 0 {
                outcome.sequence = owned_sequence();
                for saved in &mut self.formats {
                    match &mut saved.data {
                        SavedData::Bytes(bytes) => {
                            let size = bytes.len().max(1);
                            let memory = GlobalAlloc(GMEM_MOVEABLE, size);
                            if memory.is_null() {
                                continue;
                            }
                            let pointer = GlobalLock(memory);
                            if pointer.is_null() {
                                GlobalFree(memory);
                                continue;
                            }
                            if bytes.is_empty() {
                                *(pointer as *mut u8) = 0;
                            } else {
                                std::ptr::copy_nonoverlapping(
                                    bytes.as_ptr(),
                                    pointer as *mut u8,
                                    bytes.len(),
                                );
                            }
                            GlobalUnlock(memory);
                            if SetClipboardData(saved.format, memory as HANDLE).is_null() {
                                GlobalFree(memory);
                            } else {
                                outcome.succeeded = true;
                            }
                        }
                        SavedData::Bitmap(handle) => {
                            let bitmap = std::mem::replace(handle, std::ptr::null_mut());
                            if bitmap.is_null() {
                                continue;
                            }
                            if SetClipboardData(CF_BITMAP, bitmap as HANDLE).is_null() {
                                DeleteObject(bitmap as HGDIOBJ);
                            } else {
                                outcome.succeeded = true;
                            }
                        }
                    }
                }
                if self.formats.is_empty() {
                    outcome.succeeded = true;
                }
                outcome.sequence = owned_sequence();
            }
            CloseClipboard();
        }
        outcome
    }
}

/// Duplicate one clipboard format while the clipboard is open.
///
/// SAFETY: the clipboard must be open on the calling thread; `format` must have
/// been returned by EnumClipboardFormats in the same open/close cycle.
unsafe fn save_format(format: u32) -> Result<(SavedData, usize), String> {
    let handle = GetClipboardData(format);
    if handle.is_null() {
        return Err(format!(
            "无法读取剪贴板格式 {format}，改为直接输入以保留原剪贴板"
        ));
    }
    if format == CF_BITMAP {
        let mut bitmap: BITMAP = std::mem::zeroed();
        let described = GetObjectW(
            handle,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bitmap as *mut BITMAP as *mut core::ffi::c_void,
        );
        let size = if described != 0 {
            (bitmap.bmWidth as u64)
                .saturating_mul(bitmap.bmHeight as u64)
                .saturating_mul(bitmap.bmBitsPixel as u64)
                .saturating_div(8)
        } else {
            0
        };
        if size > MAX_SNAPSHOT_BYTES as u64 {
            return Err("剪贴板图片过大，改为直接输入以保留原剪贴板".to_string());
        }
        let size = size as usize;
        let copy = CopyImage(handle, IMAGE_BITMAP, 0, 0, 0);
        if copy.is_null() {
            return Err("无法复制剪贴板图片，改为直接输入以保留原剪贴板".to_string());
        }
        return Ok((SavedData::Bitmap(copy as HBITMAP), size));
    }

    let size = GlobalSize(handle as HANDLE);
    if size > MAX_SNAPSHOT_BYTES {
        return Err("剪贴板内容过大，改为直接输入以保留原剪贴板".to_string());
    }
    let pointer = GlobalLock(handle as HANDLE);
    if pointer.is_null() {
        return Err(format!(
            "无法读取剪贴板格式 {format}，改为直接输入以保留原剪贴板"
        ));
    }
    let mut bytes = vec![0u8; size];
    if size > 0 {
        std::ptr::copy_nonoverlapping(pointer as *const u8, bytes.as_mut_ptr(), size);
    }
    GlobalUnlock(handle as HANDLE);
    Ok((SavedData::Bytes(bytes), size))
}

/// Take a full copy of the current clipboard.
///
/// The clipboard is opened once, so its format list cannot change between the
/// formats that are duplicated.
pub fn snapshot() -> Result<ClipboardSnapshot, String> {
    if !open_clipboard(std::ptr::null_mut()) {
        return Err("无法读取剪贴板内容，改为直接输入以保留原剪贴板".to_string());
    }
    let mut formats = Vec::new();
    // SAFETY: the clipboard is open; EnumClipboardFormats is called with the
    // previous format (0 to start) until it returns 0.
    unsafe {
        let mut format = 0u32;
        loop {
            format = EnumClipboardFormats(format);
            if format == 0 {
                break;
            }
            formats.push(format);
            if formats.len() > MAX_CLIPBOARD_FORMATS {
                break;
            }
        }
    }
    if formats.len() > MAX_CLIPBOARD_FORMATS {
        // SAFETY: the clipboard was opened above and is closed exactly once.
        unsafe {
            CloseClipboard();
        }
        return Err("剪贴板包含过多格式，改为直接输入以保留原剪贴板".to_string());
    }
    let mut saved = Vec::with_capacity(formats.len());
    let mut total = 0usize;
    for format in formats {
        if unsupported_format(format) {
            // SAFETY: the clipboard was opened above and is closed exactly once.
            unsafe {
                CloseClipboard();
            }
            return Err(format!(
                "剪贴板格式 {format} 无法安全恢复，改为直接输入以保留原剪贴板"
            ));
        }
        // SAFETY: the clipboard is open and format was enumerated above.
        match unsafe { save_format(format) } {
            Ok((data, size)) => {
                total = total.saturating_add(size);
                if total > MAX_SNAPSHOT_BYTES {
                    // SAFETY: the clipboard was opened above; saved formats free
                    // their own GDI handles when the vector is dropped.
                    unsafe {
                        CloseClipboard();
                    }
                    return Err("剪贴板内容过大，改为直接输入以保留原剪贴板".to_string());
                }
                saved.push(SavedFormat { format, data });
            }
            Err(error) => {
                // SAFETY: the clipboard was opened above and is closed exactly
                // once before returning.
                unsafe {
                    CloseClipboard();
                }
                return Err(error);
            }
        }
    }
    // SAFETY: the clipboard was opened above and is closed exactly once.
    unsafe {
        CloseClipboard();
    }
    Ok(ClipboardSnapshot { formats: saved })
}
