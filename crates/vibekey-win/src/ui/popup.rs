//! The selectable snippet list popup.
//!
//! The window is a borderless top level window with a list box child. It is
//! shown at the text caret position, takes the foreground so that Escape and the
//! arrow keys work, and closes itself when it loses the activation.
//!
//! Clicking an item executes it immediately. Since a list box reports
//! LBN_SELCHANGE for arrow key navigation as well, the list box is subclassed to
//! react to WM_LBUTTONUP instead. Enter and double click execute the selected
//! item; Escape closes the popup.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, COLOR_WINDOW, DEFAULT_GUI_FONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::{self, wide};

const CLASS_NAME: &str = "VibekeyPopup";
const LISTBOX_ID: i32 = 100;
const WIDTH_LOGICAL: i32 = 520;
const ITEM_HEIGHT_LOGICAL: i32 = 22;
const MAX_HEIGHT_LOGICAL: i32 = 420;

/// The snippet picker. Only one instance exists and it is owned by the app.
pub struct Popup {
    hwnd: HWND,
    listbox: HWND,
    /// Indices into Config::snippet for every row of the list.
    items: Vec<usize>,
    /// Window that was in the foreground when the popup opened.
    target: HWND,
    open: bool,
}

impl Popup {
    pub fn new() -> Self {
        Self {
            hwnd: std::ptr::null_mut(),
            listbox: std::ptr::null_mut(),
            items: Vec::new(),
            target: std::ptr::null_mut(),
            open: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open && !self.hwnd.is_null()
    }

    pub fn target(&self) -> HWND {
        self.target
    }

    pub fn item(&self, index: usize) -> Option<usize> {
        self.items.get(index).copied()
    }

    /// Show the list. Returns false when the popup could not be created.
    pub fn show(&mut self, owner: HWND, items: Vec<usize>, texts: &[String], target: HWND) -> bool {
        if texts.is_empty() || items.len() != texts.len() {
            return false;
        }
        if let Err(error) = self.ensure_window(owner) {
            log::error!("could not create the snippet popup: {error}");
            return false;
        }
        let encoded: Vec<Vec<u16>> = texts.iter().map(|text| wide(text)).collect();
        // SAFETY: the list box handle is valid; the strings live for the
        // duration of the call and the list box copies them.
        unsafe {
            SendMessageW(self.listbox, LB_RESETCONTENT, 0, 0);
            for text in &encoded {
                SendMessageW(self.listbox, LB_ADDSTRING, 0, text.as_ptr() as isize);
            }
            SendMessageW(self.listbox, LB_SETCURSEL, 0, 0);
        }

        let scale = util::dpi_scale();
        let width = (WIDTH_LOGICAL as f32 * scale) as i32;
        let item_height = (ITEM_HEIGHT_LOGICAL as f32 * scale) as i32;
        let wanted_height = items.len() as i32 * item_height + util::scale(8);
        let height = wanted_height.min((MAX_HEIGHT_LOGICAL as f32 * scale) as i32);
        let position = util::clamp_to_monitor(util::caret_pos(), width, height);

        // SAFETY: the window handles are valid; the rectangle is a valid out
        // parameter.
        unsafe {
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                position.x,
                position.y,
                width,
                height,
                SWP_SHOWWINDOW,
            );
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut client);
            SetWindowPos(
                self.listbox,
                std::ptr::null_mut(),
                0,
                0,
                client.right,
                client.bottom,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            ShowWindow(self.hwnd, SW_SHOW);
            SetForegroundWindow(self.hwnd);
            SetFocus(self.listbox);
        }

        self.items = items;
        self.target = target;
        self.open = true;
        true
    }

    pub fn hide(&mut self) {
        self.open = false;
        if !self.hwnd.is_null() {
            // SAFETY: the window handle is valid and merely hidden.
            unsafe {
                ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    fn ensure_window(&mut self, owner: HWND) -> Result<(), String> {
        if !ensure_class() {
            return Err("RegisterClassW failed".to_string());
        }
        if !self.hwnd.is_null() {
            return Ok(());
        }
        // SAFETY: the class is registered and the strings are valid for the
        // duration of the call.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = wide(CLASS_NAME);
        let title = wide("Vibekey 片段");
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP | WS_BORDER,
                0,
                0,
                100,
                100,
                owner,
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err("CreateWindowExW failed".to_string());
        }
        self.hwnd = hwnd;

        let listbox_class = wide("LISTBOX");
        let empty = wide("");
        let listbox = unsafe {
            CreateWindowExW(
                0,
                listbox_class.as_ptr(),
                empty.as_ptr(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | LBS_NOTIFY as u32
                    | LBS_NOINTEGRALHEIGHT as u32,
                0,
                0,
                100,
                100,
                hwnd,
                LISTBOX_ID as usize as *mut core::ffi::c_void,
                instance,
                std::ptr::null(),
            )
        };
        if listbox.is_null() {
            return Err("could not create the list box".to_string());
        }
        self.listbox = listbox;
        // SAFETY: the controls are live windows of this process.
        unsafe {
            let font = GetStockObject(DEFAULT_GUI_FONT);
            if !font.is_null() {
                SendMessageW(listbox, WM_SETFONT, font as usize, 1);
            }
            let old =
                util::replace_window_proc(listbox, listbox_proc as *const () as usize as isize);
            OLD_LIST_PROC.store(old, Ordering::Release);
        }
        Ok(())
    }
}

impl Default for Popup {
    fn default() -> Self {
        Self::new()
    }
}

static REGISTERED: AtomicBool = AtomicBool::new(false);
static OLD_LIST_PROC: AtomicIsize = AtomicIsize::new(0);

fn ensure_class() -> bool {
    if REGISTERED.load(Ordering::Acquire) {
        return true;
    }
    // SAFETY: the class name lives for the duration of the call; RegisterClassW
    // copies it.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide(CLASS_NAME);
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.style = 0;
        class.lpfnWndProc = Some(popup_proc);
        class.hInstance = instance;
        class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        class.hbrBackground = (COLOR_WINDOW + 1) as usize as *mut core::ffi::c_void;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return false;
        }
    }
    REGISTERED.store(true, Ordering::Release);
    true
}

/// Handler of the popup window itself.
unsafe extern "system" fn popup_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ACTIVATE => {
            // Closing on WA_INACTIVE instead of WM_KILLFOCUS keeps the popup
            // open while the list box child owns the focus.
            if (wparam as u32 & 0xFFFF) == WA_INACTIVE {
                crate::app::app_mut().popup.hide();
            }
            0
        }
        WM_LBUTTONUP => {
            crate::app::app_mut().popup.hide();
            0
        }
        WM_CLOSE => {
            crate::app::app_mut().popup.hide();
            0
        }
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Handler of the list box child. Runs the snippet on click, Enter or Escape.
unsafe extern "system" fn listbox_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_KEYDOWN => {
            let key = wparam as u16;
            if key == VK_RETURN {
                execute_selected(hwnd);
                return 0;
            }
            if key == VK_ESCAPE {
                crate::app::app_mut().popup.hide();
                return 0;
            }
        }
        WM_LBUTTONUP => {
            if execute_clicked(hwnd, lparam) {
                return 0;
            }
        }
        WM_LBUTTONDBLCLK => {
            if execute_clicked(hwnd, lparam) {
                return 0;
            }
        }
        _ => {}
    }
    let old = OLD_LIST_PROC.load(Ordering::Acquire);
    if old == 0 {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    // SAFETY: the stored value is the window procedure the list box class
    // installed originally and it is called with the documented arguments.
    let previous: WNDPROC = std::mem::transmute(old);
    CallWindowProcW(previous, hwnd, message, wparam, lparam)
}

unsafe fn execute_clicked(hwnd: HWND, lparam: LPARAM) -> bool {
    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    let packed = (((y as u16 as u32) << 16) | (x as u16 as u32)) as isize;
    let result = SendMessageW(hwnd, LB_ITEMFROMPOINT, 0, packed);
    let outside = ((result >> 16) & 0xFFFF) != 0;
    if outside {
        return false;
    }
    let index = (result & 0xFFFF) as usize;
    execute_index(index);
    true
}

unsafe fn execute_selected(hwnd: HWND) {
    let selection = SendMessageW(hwnd, LB_GETCURSEL, 0, 0);
    if selection >= 0 {
        execute_index(selection as usize);
    } else {
        crate::app::app_mut().popup.hide();
    }
}

/// Hide the popup and hand the selected snippet to the main window, which
/// expands it and queues the injection.
fn execute_index(list_index: usize) {
    // SAFETY: list box messages are only handled on the main thread while the
    // app exists.
    let app = unsafe { crate::app::app_mut() };
    let target = app.popup.target();
    let snippet_index = app.popup.item(list_index);
    app.popup.hide();
    if let Some(index) = snippet_index {
        if !target.is_null() {
            // SAFETY: the target window handle is either null or a window that
            // was in the foreground when the popup opened.
            unsafe {
                SetForegroundWindow(target);
            }
        }
        app.inject_snippet(index, target);
    }
}
