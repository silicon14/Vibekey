//! Clipboard text helpers.
//!
//! Only CF_UNICODETEXT is read and written: the clipboard is opened with retries
//! because another application may hold it for a few milliseconds. The existing
//! formats are inspected first so that richer clipboard content (files, images,
//! HTML, RTF) is never destroyed by a paste.

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

/// CF_UNICODETEXT; defined here because the windows-sys module that owns the
/// clipboard format constants (Win32_System_Ole) is not enabled.
pub const CF_UNICODETEXT: u32 = 13;
/// Clipboard formats that only carry plain text and may be replaced by a paste.
const CF_TEXT: u32 = 1;
const CF_OEMTEXT: u32 = 7;
/// Language information that accompanies a text clipboard entry.
const CF_LOCALE: u32 = 16;

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

/// What the clipboard currently holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    /// No formats at all.
    Empty,
    /// Only plain text formats; the text, when the owner provided one.
    Text(Option<String>),
    /// Files, images, HTML, RTF or any other non plain text format.
    Rich,
}

/// Inspect the clipboard in one open/close cycle.
///
/// Returns None when the clipboard could not be opened, so callers can decide
/// whether it is safe to continue (a paste needs the save/restore information).
pub fn inspect() -> Option<ClipboardContent> {
    if !open_clipboard(std::ptr::null_mut()) {
        return None;
    }
    let mut formats = Vec::new();
    // SAFETY: the clipboard is open; EnumClipboardFormats is called with the
    // previous format (0 to start) until it returns 0. The clipboard is closed
    // exactly once below.
    unsafe {
        let mut format = 0u32;
        loop {
            format = EnumClipboardFormats(format);
            if format == 0 {
                break;
            }
            formats.push(format);
            if formats.len() > 256 {
                break;
            }
        }
        let rich = formats
            .iter()
            .any(|format| !matches!(*format, CF_TEXT | CF_OEMTEXT | CF_UNICODETEXT | CF_LOCALE));
        let text = if rich || formats.is_empty() {
            None
        } else {
            read_text_locked()
        };
        CloseClipboard();
        if rich {
            Some(ClipboardContent::Rich)
        } else if formats.is_empty() {
            Some(ClipboardContent::Empty)
        } else {
            Some(ClipboardContent::Text(text))
        }
    }
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

/// Empty the clipboard again, used to restore the "no clipboard content" state.
pub fn clear() -> ClipboardMutation {
    if !open_clipboard(std::ptr::null_mut()) {
        return ClipboardMutation::failed();
    }
    // SAFETY: the clipboard is open and is closed exactly once below. Sequence
    // attribution happens before CloseClipboard while ownership is exclusive.
    unsafe {
        let mut outcome = ClipboardMutation::failed();
        if EmptyClipboard() != 0 {
            outcome.succeeded = true;
            outcome.sequence = owned_sequence();
        }
        CloseClipboard();
        outcome
    }
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
