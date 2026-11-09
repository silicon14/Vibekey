//! Modern native snippet picker.
//!
//! The popup is a borderless, light themed Win32 window.  A cue-banner edit
//! filters the snippets, an owner-drawn list presents shortcut/name/preview
//! rows, and the footer exposes snippet creation and editing commands.  All
//! controls and window procedures run on the application's main thread.
//!
//! Clicking a row inserts it immediately.  Arrow-key selection only changes the
//! current row because mouse activation is handled by the list subclass on
//! `WM_LBUTTONUP`, rather than by `LBN_SELCHANGE`.  Enter and double click also
//! insert, Escape closes, and F2/right click edit the selected snippet.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FrameRect, InvalidateRect,
    SelectObject, SetBkColor, SetBkMode, SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_SETCUEBANNER, MEASUREITEMSTRUCT, ODS_SELECTED,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, SetFocus, VK_DOWN, VK_ESCAPE, VK_F2, VK_RETURN, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::ui::theme;
use crate::util::{self, wide};

const CLASS_NAME: &str = "VibekeyPopup";

const SEARCH_ID: i32 = 100;
const LISTBOX_ID: i32 = 101;
const TITLE_ID: i32 = 102;
const EMPTY_ID: i32 = 103;
const COUNT_ID: i32 = 104;
const NEW_BUTTON_ID: i32 = 110;
const EDIT_BUTTON_ID: i32 = 111;

const WIDTH_LOGICAL: i32 = 600;
const MAX_HEIGHT_LOGICAL: i32 = 600;
const ITEM_HEIGHT_LOGICAL: i32 = 56;
const MAX_VISIBLE_ROWS: usize = 8;
const LIST_TOP_LOGICAL: i32 = 96;
const LIST_CHROME_LOGICAL: i32 = 152;
const PREVIEW_CHARS: usize = 48;

// Static-control styles from WinUser.h.  Defining these two values locally
// avoids enabling another windows-sys feature solely for style constants.
const STATIC_CENTER: u32 = 0x0001;
const STATIC_CENTER_IMAGE: u32 = 0x0200;

/// One snippet shown by the picker.
///
/// `snippet_index` always refers to the original `Config::snippet` vector, not
/// to a filtered list row.  The display strings are detached from the config so
/// filtering and owner drawing never borrow application configuration data.
#[derive(Debug, Clone)]
pub struct PopupItem {
    pub snippet_index: usize,
    pub key: String,
    pub name: String,
    pub preview: String,
}

impl PopupItem {
    pub fn from_snippet(index: usize, snippet: &vibekey_core::Snippet) -> Self {
        let name = snippet.name.trim();
        let first_line = snippet.text.lines().next().unwrap_or("");
        let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview = if collapsed.chars().count() > PREVIEW_CHARS {
            let mut preview = collapsed
                .chars()
                .take(PREVIEW_CHARS - 1)
                .collect::<String>();
            preview.push('…');
            preview
        } else {
            collapsed
        };

        Self {
            snippet_index: index,
            key: snippet.key.trim().to_string(),
            name: if name.is_empty() {
                "（无名称）".to_string()
            } else {
                name.to_string()
            },
            preview,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Controls {
    title: HWND,
    search: HWND,
    listbox: HWND,
    empty: HWND,
    count: HWND,
    new_button: HWND,
    edit_button: HWND,
}

/// The snippet picker. Only one instance exists and it is owned by the app.
pub struct Popup {
    hwnd: HWND,
    controls: Controls,
    /// Every item supplied to `show`, in its original order.
    items: Vec<PopupItem>,
    /// Indices into `items` for the currently visible list rows.
    visible: Vec<usize>,
    /// Window that was selected as the injection target when the popup opened.
    target: HWND,
    open: bool,
    /// Prevents the programmatic search reset in `show` from filtering twice.
    suppress_search_change: bool,
}

impl Popup {
    pub fn new() -> Self {
        Self {
            hwnd: std::ptr::null_mut(),
            controls: Controls::default(),
            items: Vec::new(),
            visible: Vec::new(),
            target: std::ptr::null_mut(),
            open: false,
            suppress_search_change: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open && !self.hwnd.is_null()
    }

    pub fn target(&self) -> HWND {
        self.target
    }

    /// Map a filtered list row back to its original `Config::snippet` index.
    pub fn item(&self, index: usize) -> Option<usize> {
        self.visible_item(index).map(|item| item.snippet_index)
    }

    fn visible_item(&self, index: usize) -> Option<&PopupItem> {
        let item_index = *self.visible.get(index)?;
        self.items.get(item_index)
    }

    /// Show the picker. Returns false only when its Win32 window cannot be made.
    pub fn show(&mut self, owner: HWND, items: Vec<PopupItem>, target: HWND) -> bool {
        if let Err(error) = self.ensure_window(owner) {
            log::error!("could not create the snippet popup: {error}");
            return false;
        }

        self.items = items;
        self.target = target;
        self.open = true;

        // A new picker session always starts with the complete list.  Suppress
        // EN_CHANGE because SetWindowTextW dispatches it synchronously.
        self.suppress_search_change = true;
        set_control_text(self.controls.search, "");
        self.suppress_search_change = false;
        self.apply_filter("");

        let anchor = util::caret_pos();
        let work = util::monitor_work_area(anchor);
        let work_width = work.right - work.left;
        let work_height = work.bottom - work.top;
        let width =
            util::scale(WIDTH_LOGICAL).min((work_width - util::scale(8)).max(util::scale(1)));
        let rows = self.visible.len().clamp(1, MAX_VISIBLE_ROWS) as i32;
        let wanted_height =
            util::scale(LIST_CHROME_LOGICAL) + rows * util::scale(ITEM_HEIGHT_LOGICAL);
        // Limit both dimensions before positioning. On high-DPI displays the
        // logical 600px design can otherwise be taller than the physical work
        // area, putting the edit/new buttons off screen.
        let height = wanted_height
            .min(util::scale(MAX_HEIGHT_LOGICAL))
            .min((work_height - util::scale(8)).max(util::scale(1)));
        let position = util::clamp_to_monitor(anchor, width, height);

        // SAFETY: the popup handle is live and the coordinates/size describe a
        // physical-pixel rectangle clamped to the nearest monitor's work area.
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
            ShowWindow(self.hwnd, SW_SHOW);
        }

        // SetForegroundWindow alone is often refused after a global hotkey.
        // force_foreground temporarily attaches the input queues, preserving the
        // established keyboard-focus behaviour of the original picker.
        let activated = util::force_foreground(self.hwnd);
        if !activated {
            log::warn!("could not take the foreground for the snippet list");
            // Keep the picker visible above its target even if activation fails.
            // SAFETY: the popup handle is live; only its Z order is changed.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                );
            }
        }

        // Search is the natural initial focus for a filterable picker.  The list
        // already has row zero selected, so Enter works without focusing it.
        // SAFETY: the search control is a live child of the active popup.
        unsafe {
            SetFocus(self.controls.search);
        }
        true
    }

    pub fn hide(&mut self) {
        self.open = false;
        if !self.hwnd.is_null() {
            // SAFETY: the popup handle is live and is only hidden, not destroyed.
            unsafe {
                ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    /// Rebuild the filtered row mapping and all list-dependent controls.
    fn apply_filter(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        self.visible = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches_query(item, &query))
            .map(|(index, _)| index)
            .collect();

        let empty = wide("");
        // SAFETY: the list is a live owner-drawn list box.  It copies every
        // string because LBS_HASSTRINGS is set; row text itself comes from
        // `visible_item` during WM_DRAWITEM, not from these empty labels.
        unsafe {
            SendMessageW(self.controls.listbox, WM_SETREDRAW, 0, 0);
            SendMessageW(self.controls.listbox, LB_RESETCONTENT, 0, 0);
            for _ in &self.visible {
                SendMessageW(
                    self.controls.listbox,
                    LB_ADDSTRING,
                    0,
                    empty.as_ptr() as isize,
                );
            }
            SendMessageW(self.controls.listbox, LB_SETTOPINDEX, 0, 0);
            if self.visible.is_empty() {
                SendMessageW(self.controls.listbox, LB_SETCURSEL, usize::MAX, 0);
            } else {
                SendMessageW(self.controls.listbox, LB_SETCURSEL, 0, 0);
            }
            SendMessageW(self.controls.listbox, WM_SETREDRAW, 1, 0);
            InvalidateRect(self.controls.listbox, std::ptr::null(), 1);
        }

        let has_rows = !self.visible.is_empty();
        // SAFETY: both handles are live children; only visibility is changed.
        unsafe {
            ShowWindow(
                self.controls.listbox,
                if has_rows { SW_SHOW } else { SW_HIDE },
            );
            ShowWindow(
                self.controls.empty,
                if has_rows { SW_HIDE } else { SW_SHOW },
            );
            EnableWindow(self.controls.edit_button, has_rows as i32);
        }

        let empty_text = if self.items.is_empty() {
            "还没有片段\r\n点击“新建片段”创建第一个片段"
        } else {
            "没有匹配的片段\r\n试试其他关键词，或按 Esc 关闭"
        };
        set_control_text(self.controls.empty, empty_text);
        let count_text = if self.visible.len() == self.items.len() {
            format!("共 {} 个片段", self.items.len())
        } else {
            format!(
                "{} 个结果（共 {} 个）",
                self.visible.len(),
                self.items.len()
            )
        };
        set_control_text(self.controls.count, &count_text);
    }

    fn ensure_window(&mut self, owner: HWND) -> Result<(), String> {
        if !ensure_class() {
            return Err("RegisterClassW failed".to_string());
        }
        if !self.hwnd.is_null() {
            return Ok(());
        }

        // SAFETY: GetModuleHandleW only queries this process's executable.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = wide(CLASS_NAME);
        let title = wide("Vibekey 片段选择器");
        // SAFETY: the registered class and NUL-terminated strings remain valid
        // for the call.  WS_POPUP deliberately has no system WS_BORDER.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP | WS_CLIPCHILDREN,
                0,
                0,
                util::scale(WIDTH_LOGICAL),
                util::scale(MAX_HEIGHT_LOGICAL),
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

        self.controls.title = create_control(
            hwnd,
            instance,
            "STATIC",
            "搜索并选择片段",
            WS_CHILD | WS_VISIBLE | STATIC_CENTER_IMAGE,
            0,
            TITLE_ID,
        );
        self.controls.search = create_control(
            hwnd,
            instance,
            "EDIT",
            "",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            SEARCH_ID,
        );
        self.controls.listbox = create_control(
            hwnd,
            instance,
            "LISTBOX",
            "",
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_OWNERDRAWFIXED as u32
                | LBS_HASSTRINGS as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            0,
            LISTBOX_ID,
        );
        self.controls.empty = create_control(
            hwnd,
            instance,
            "STATIC",
            "没有匹配的片段",
            WS_CHILD | STATIC_CENTER | STATIC_CENTER_IMAGE,
            0,
            EMPTY_ID,
        );
        self.controls.count = create_control(
            hwnd,
            instance,
            "STATIC",
            "",
            WS_CHILD | WS_VISIBLE | STATIC_CENTER_IMAGE,
            0,
            COUNT_ID,
        );
        self.controls.new_button = create_control(
            hwnd,
            instance,
            "BUTTON",
            "新建片段",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            NEW_BUTTON_ID,
        );
        self.controls.edit_button = create_control(
            hwnd,
            instance,
            "BUTTON",
            "编辑所选(F2)",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            EDIT_BUTTON_ID,
        );

        let controls = self.controls;
        if [
            controls.title,
            controls.search,
            controls.listbox,
            controls.empty,
            controls.count,
            controls.new_button,
            controls.edit_button,
        ]
        .iter()
        .any(|handle| handle.is_null())
        {
            // SAFETY: creation of at least one child failed, so the hidden popup
            // and any children already created are destroyed together.
            unsafe {
                DestroyWindow(hwnd);
            }
            self.hwnd = std::ptr::null_mut();
            self.controls = Controls::default();
            return Err("could not create one or more popup controls".to_string());
        }

        theme::set_font(controls.title, theme::title_font());
        theme::set_font(controls.search, theme::body_font());
        theme::set_font(controls.empty, theme::body_font());
        theme::set_font(controls.count, theme::small_font());
        theme::set_font(controls.new_button, theme::semibold_font());
        theme::set_font(controls.edit_button, theme::body_font());

        let cue = wide("搜索名称、快捷键或内容预览…");
        // SAFETY: all child handles are live, shared theme fonts outlive them,
        // and the cue string is copied synchronously by the edit control.
        unsafe {
            SendMessageW(controls.search, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
            SendMessageW(
                controls.listbox,
                LB_SETITEMHEIGHT,
                0,
                util::scale(ITEM_HEIGHT_LOGICAL) as isize,
            );
            let old_list = util::replace_window_proc(
                controls.listbox,
                listbox_proc as *const () as usize as isize,
            );
            OLD_LIST_PROC.store(old_list, Ordering::Release);
            let old_search = util::replace_window_proc(
                controls.search,
                search_proc as *const () as usize as isize,
            );
            OLD_SEARCH_PROC.store(old_search, Ordering::Release);
        }
        Ok(())
    }
}

impl Default for Popup {
    fn default() -> Self {
        Self::new()
    }
}

/// Case-insensitive substring filtering over name, shortcut and content preview.
/// Multiple whitespace-separated terms must all match.  `Alt+1` also matches a
/// snippet whose stored key is just `1`.
fn matches_query(item: &PopupItem, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} alt+{}",
        item.name.to_lowercase(),
        item.key.to_lowercase(),
        item.preview.to_lowercase(),
        item.key.to_lowercase()
    );
    query.split_whitespace().all(|term| haystack.contains(term))
}

static REGISTERED: AtomicBool = AtomicBool::new(false);
static OLD_LIST_PROC: AtomicIsize = AtomicIsize::new(0);
static OLD_SEARCH_PROC: AtomicIsize = AtomicIsize::new(0);

fn ensure_class() -> bool {
    if REGISTERED.load(Ordering::Acquire) {
        return true;
    }
    // SAFETY: all pointers reference live stack/static data for the call;
    // RegisterClassW copies the class name and the shared brush is process-lived.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide(CLASS_NAME);
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(popup_proc);
        class.hInstance = instance;
        class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        class.hbrBackground = theme::surface_brush();
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return false;
        }
    }
    REGISTERED.store(true, Ordering::Release);
    true
}

/// Create one standard Win32 child control without a system border.
fn create_control(
    parent: HWND,
    instance: windows_sys::Win32::Foundation::HMODULE,
    class_name: &str,
    text: &str,
    style: u32,
    ex_style: u32,
    id: i32,
) -> HWND {
    let class_name = wide(class_name);
    let text = wide(text);
    // SAFETY: the parent/module are valid, both strings are NUL terminated and
    // live for the call, and `id` is encoded as the documented child HMENU.
    unsafe {
        CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            text.as_ptr(),
            style,
            0,
            0,
            0,
            0,
            parent,
            id as usize as *mut core::ffi::c_void,
            instance,
            std::ptr::null(),
        )
    }
}

fn set_control_text(hwnd: HWND, text: &str) {
    if hwnd.is_null() {
        return;
    }
    let text = wide(text);
    // SAFETY: the control handle is live and the UTF-16 buffer is NUL terminated.
    unsafe {
        SetWindowTextW(hwnd, text.as_ptr());
    }
}

fn control_text(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return String::new();
    }
    // SAFETY: GetWindowTextLengthW only queries the live control.
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    // SAFETY: the buffer has `length + 1` writable UTF-16 code units and its
    // actual capacity is passed to GetWindowTextW.
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
}

/// Hide the popup if the application instance still exists.
fn hide_popup() {
    // SAFETY: window procedures run only on the main thread.  During shutdown
    // the app pointer may already be cleared, in which case there is no popup.
    if let Some(app) = unsafe { crate::app::try_app_mut() } {
        app.popup.hide();
    }
}

/// Handler of the top-level popup and owner-draw notifications.
unsafe extern "system" fn popup_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ACTIVATE => {
            // Child focus changes do not deactivate the top-level popup.
            if (wparam as u32 & 0xFFFF) == WA_INACTIVE {
                hide_popup();
            }
            0
        }
        WM_SIZE => {
            let width = (lparam as u32 & 0xFFFF) as i32;
            let height = ((lparam as u32 >> 16) & 0xFFFF) as i32;
            layout_controls(hwnd, width, height);
            0
        }
        WM_MEASUREITEM => {
            // SAFETY: Win32 supplies a valid MEASUREITEMSTRUCT for this message;
            // the null check also tolerates malformed synthetic messages.
            let measure = lparam as *mut MEASUREITEMSTRUCT;
            if !measure.is_null() && unsafe { (*measure).CtlID } == LISTBOX_ID as u32 {
                // SAFETY: the pointer was validated above and points to the
                // writable structure supplied by Win32.
                unsafe {
                    (*measure).itemHeight = util::scale(ITEM_HEIGHT_LOGICAL) as u32;
                }
                return 1;
            }
            0
        }
        WM_DRAWITEM => {
            // SAFETY: Win32 supplies a read-only DRAWITEMSTRUCT for this message.
            let draw = lparam as *const DRAWITEMSTRUCT;
            if draw.is_null() || unsafe { (*draw).CtlID } != LISTBOX_ID as u32 {
                return 0;
            }
            // SAFETY: the pointer was checked and remains valid for WM_DRAWITEM.
            unsafe {
                draw_list_item(&*draw);
            }
            1
        }
        WM_COMMAND => {
            handle_command(hwnd, wparam);
            0
        }
        WM_CTLCOLOREDIT => {
            // SAFETY: wparam is the edit control's DC for this message.
            unsafe {
                SetBkColor(wparam as _, theme::SURFACE);
                SetTextColor(wparam as _, theme::TEXT);
            }
            theme::surface_brush() as isize
        }
        WM_CTLCOLORSTATIC => {
            // SAFETY: wparam is the static control's DC for this message.
            unsafe {
                SetBkMode(wparam as _, TRANSPARENT as i32);
                SetTextColor(wparam as _, theme::TEXT_MUTED);
            }
            theme::surface_brush() as isize
        }
        WM_CTLCOLORBTN => theme::surface_brush() as isize,
        WM_ERASEBKGND => {
            let mut client = util::rect(0, 0, 0, 0);
            // SAFETY: client is a valid out rectangle and wparam is the DC
            // supplied with WM_ERASEBKGND.
            unsafe {
                GetClientRect(hwnd, &mut client);
            }
            theme::fill(wparam as _, &client, theme::SURFACE);
            1
        }
        WM_PAINT => {
            paint_popup(hwnd);
            0
        }
        WM_LBUTTONUP => {
            // Preserve the original click-on-empty-chrome behaviour.  Clicks on
            // child controls are dispatched to those children instead.
            hide_popup();
            0
        }
        WM_CLOSE => {
            hide_popup();
            0
        }
        _ => {
            // SAFETY: unhandled messages use the default procedure with the
            // original arguments supplied by Win32.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn handle_command(hwnd: HWND, wparam: WPARAM) {
    let id = (wparam as u32 & 0xFFFF) as i32;
    let notification = (wparam as u32 >> 16) & 0xFFFF;
    match (id, notification) {
        (SEARCH_ID, EN_CHANGE) => {
            // SAFETY: this is a main-thread control notification; the app may be
            // absent only while the process is shutting down.
            if let Some(app) = unsafe { crate::app::try_app_mut() } {
                if !app.popup.suppress_search_change {
                    let query = control_text(app.popup.controls.search);
                    app.popup.apply_filter(&query);
                }
            }
        }
        (SEARCH_ID, EN_SETFOCUS) | (SEARCH_ID, EN_KILLFOCUS) => {
            // SAFETY: a null rectangle invalidates the whole popup so the search
            // frame can repaint its focus colour.  No background erase is needed.
            unsafe {
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
        }
        (NEW_BUTTON_ID, BN_CLICKED) => open_new_snippet(),
        (EDIT_BUTTON_ID, BN_CLICKED) => edit_selected(),
        _ => {}
    }
}

/// DPI-aware layout shared by initial WM_SIZE and subsequent window resizing.
fn layout_controls(hwnd: HWND, width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    let margin = util::scale(16);
    let search_top = util::scale(50);
    let search_height = util::scale(36);
    let list_top = util::scale(LIST_TOP_LOGICAL);
    let footer_height = util::scale(56);
    let list_height = (height - list_top - footer_height).max(util::scale(ITEM_HEIGHT_LOGICAL));
    let content_width = (width - 2 * margin).max(0);

    move_control(
        hwnd,
        TITLE_ID,
        margin,
        util::scale(12),
        content_width,
        util::scale(30),
    );
    move_control(
        hwnd,
        SEARCH_ID,
        margin + util::scale(10),
        search_top + util::scale(6),
        (content_width - 2 * util::scale(10)).max(0),
        search_height - 2 * util::scale(6),
    );
    move_control(
        hwnd,
        LISTBOX_ID,
        margin,
        list_top,
        content_width,
        list_height,
    );
    move_control(hwnd, EMPTY_ID, margin, list_top, content_width, list_height);

    let button_height = util::scale(30);
    let button_y = height - util::scale(42);
    let new_width = util::scale(100);
    let edit_width = util::scale(120);
    let button_gap = util::scale(8);
    let edit_x = width - margin - edit_width;
    let new_x = edit_x - button_gap - new_width;
    move_control(
        hwnd,
        NEW_BUTTON_ID,
        new_x,
        button_y,
        new_width,
        button_height,
    );
    move_control(
        hwnd,
        EDIT_BUTTON_ID,
        edit_x,
        button_y,
        edit_width,
        button_height,
    );
    move_control(
        hwnd,
        COUNT_ID,
        margin,
        height - util::scale(39),
        (new_x - margin - util::scale(12)).max(0),
        util::scale(24),
    );
}

fn move_control(parent: HWND, id: i32, x: i32, y: i32, width: i32, height: i32) {
    // SAFETY: GetDlgItem only reads the live popup's child table.
    let control = unsafe { GetDlgItem(parent, id) };
    if control.is_null() {
        return;
    }
    // SAFETY: the child handle was returned by GetDlgItem and all dimensions are
    // physical client coordinates.  Repainting keeps filtered/empty states aligned.
    unsafe {
        MoveWindow(control, x, y, width, height, 1);
    }
}

fn paint_popup(hwnd: HWND) {
    // SAFETY: paint is a valid out structure; BeginPaint/EndPaint are paired for
    // this live popup and all drawing uses the returned DC.
    unsafe {
        let mut paint: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut paint);
        if hdc.is_null() {
            EndPaint(hwnd, &paint);
            return;
        }
        let mut client = util::rect(0, 0, 0, 0);
        GetClientRect(hwnd, &mut client);
        theme::fill(hdc, &client, theme::SURFACE);

        let margin = util::scale(16);
        let search_frame = util::rect(
            margin,
            util::scale(50),
            client.right - margin,
            util::scale(86),
        );
        let search = GetDlgItem(hwnd, SEARCH_ID);
        let focused = !search.is_null() && GetFocus() == search;
        theme::fill_rounded(
            hdc,
            &search_frame,
            util::scale(8),
            if focused {
                theme::ACCENT
            } else {
                theme::BORDER
            },
        );
        let border = util::scale(1).max(1);
        let inner = util::rect(
            search_frame.left + border,
            search_frame.top + border,
            search_frame.right - border,
            search_frame.bottom - border,
        );
        theme::fill_rounded(hdc, &inner, util::scale(7), theme::SURFACE);

        let separator_y = client.bottom - util::scale(56);
        let separator = util::rect(margin, separator_y, client.right - margin, separator_y + 1);
        theme::fill(hdc, &separator, theme::BORDER);

        let brush = CreateSolidBrush(theme::BORDER);
        if !brush.is_null() {
            FrameRect(hdc, &client, brush);
            DeleteObject(brush);
        }
        EndPaint(hwnd, &paint);
    }
}

fn draw_list_item(draw: &DRAWITEMSTRUCT) {
    theme::fill(draw.hDC, &draw.rcItem, theme::SURFACE);
    if draw.itemID == u32::MAX {
        return;
    }
    // Clone the small display model so no App reference remains borrowed while
    // GDI calls are made.  The app can be absent during shutdown.
    // SAFETY: owner drawing is dispatched on the main thread.
    let item = unsafe { crate::app::try_app_mut() }
        .and_then(|app| app.popup.visible_item(draw.itemID as usize).cloned());
    let Some(item) = item else {
        return;
    };

    let selected = draw.itemState & ODS_SELECTED != 0;
    let row = draw.rcItem;
    if selected {
        let inset = util::scale(2);
        let selection = util::rect(
            row.left + inset,
            row.top + inset,
            row.right - inset,
            row.bottom - inset,
        );
        theme::fill_rounded(draw.hDC, &selection, util::scale(6), theme::ACCENT_SOFT);
    }

    let chip = util::rect(
        row.left + util::scale(10),
        row.top + util::scale(15),
        row.left + util::scale(80),
        row.top + util::scale(41),
    );
    theme::fill_rounded(
        draw.hDC,
        &chip,
        util::scale(6),
        if selected {
            theme::ACCENT
        } else {
            theme::ACCENT_SOFT
        },
    );
    let shortcut = if item.key.is_empty() {
        "无快捷键".to_string()
    } else {
        format!("Alt+{}", item.key)
    };
    draw_centred_text(
        draw.hDC,
        &chip,
        &shortcut,
        if selected {
            theme::SURFACE
        } else {
            theme::ACCENT_DARK
        },
    );

    let text_left = row.left + util::scale(92);
    let text_right = row.right - util::scale(12);
    if text_right <= text_left {
        return;
    }
    let name_rect = util::rect(
        text_left,
        row.top + util::scale(6),
        text_right,
        row.top + util::scale(28),
    );
    let preview_rect = util::rect(
        text_left,
        row.top + util::scale(30),
        text_right,
        row.top + util::scale(50),
    );
    theme::text(
        draw.hDC,
        &name_rect,
        &item.name,
        theme::semibold_font(),
        theme::TEXT,
        true,
    );
    let preview = if item.preview.is_empty() {
        "（无内容预览）"
    } else {
        item.preview.as_str()
    };
    theme::text(
        draw.hDC,
        &preview_rect,
        preview,
        theme::small_font(),
        theme::TEXT_MUTED,
        true,
    );
}

/// Centred text used only for the shortcut chip; all other text uses theme::text.
fn draw_centred_text(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    value: &str,
    color: windows_sys::Win32::Foundation::COLORREF,
) {
    let mut draw_rect = *rect;
    let value = wide(value);
    // SAFETY: the DC/rectangle belong to the active owner-draw operation, the
    // UTF-16 buffer is NUL terminated, and the shared font is process-lived.
    unsafe {
        let old_font = SelectObject(hdc, theme::small_font());
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

/// Search keyboard behaviour: Escape closes, Enter inserts the current list
/// selection, Down moves to the list, and F2 edits that selection.
unsafe extern "system" fn search_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_KEYDOWN {
        match wparam as u16 {
            VK_ESCAPE => {
                hide_popup();
                return 0;
            }
            VK_RETURN => {
                execute_selected();
                return 0;
            }
            VK_DOWN | VK_UP => {
                focus_list();
                return 0;
            }
            VK_F2 => {
                edit_selected();
                return 0;
            }
            _ => {}
        }
    }
    call_old_search_proc(hwnd, message, wparam, lparam)
}

/// List keyboard/mouse behaviour, including immediate single-click insertion.
unsafe extern "system" fn listbox_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_KEYDOWN => match wparam as u16 {
            VK_RETURN => {
                execute_selected();
                return 0;
            }
            VK_ESCAPE => {
                hide_popup();
                return 0;
            }
            VK_F2 => {
                edit_selected();
                return 0;
            }
            _ => {}
        },
        WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
            if let Some(index) = clicked_index(hwnd, lparam) {
                execute_index(index);
                return 0;
            }
        }
        WM_RBUTTONUP => {
            if let Some(index) = clicked_index(hwnd, lparam) {
                // SAFETY: the live list receives a valid zero-based row index.
                unsafe {
                    SendMessageW(hwnd, LB_SETCURSEL, index, 0);
                }
                edit_index(index);
                return 0;
            }
        }
        _ => {}
    }
    call_old_list_proc(hwnd, message, wparam, lparam)
}

unsafe fn call_old_list_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let old = OLD_LIST_PROC.load(Ordering::Acquire);
    if old == 0 {
        // SAFETY: fallback for a missing subclass predecessor.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: the atomic stores the original LISTBOX WNDPROC installed by
    // Windows, and CallWindowProcW receives the unchanged message arguments.
    let previous: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: `previous` is the validated system list procedure described above.
    unsafe { CallWindowProcW(previous, hwnd, message, wparam, lparam) }
}

unsafe fn call_old_search_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let old = OLD_SEARCH_PROC.load(Ordering::Acquire);
    if old == 0 {
        // SAFETY: fallback for a missing subclass predecessor.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: the atomic stores the original EDIT WNDPROC installed by Windows.
    let previous: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: `previous` is called with the unchanged edit-message arguments.
    unsafe { CallWindowProcW(previous, hwnd, message, wparam, lparam) }
}

/// Resolve a list mouse position without relying on LBN_SELCHANGE.
fn clicked_index(hwnd: HWND, lparam: LPARAM) -> Option<usize> {
    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    let packed = (((y as u16 as u32) << 16) | (x as u16 as u32)) as isize;
    // SAFETY: LB_ITEMFROMPOINT reads the live list and the packed signed client
    // coordinates use the documented LOWORD/HIWORD representation.
    let result = unsafe { SendMessageW(hwnd, LB_ITEMFROMPOINT, 0, packed) };
    if ((result >> 16) & 0xFFFF) != 0 {
        return None;
    }
    let index = (result & 0xFFFF) as usize;
    // LB_ITEMFROMPOINT returns the nearest item even below the last row when
    // the point is inside the list client. Validate the actual item rectangle
    // so clicking unused space after filtering cannot insert or edit that row.
    // SAFETY: LB_GETCOUNT only queries the live list control.
    let count = unsafe { SendMessageW(hwnd, LB_GETCOUNT, 0, 0) };
    if count < 0 || index >= count as usize {
        return None;
    }
    let mut item_rect = util::rect(0, 0, 0, 0);
    // SAFETY: item_rect is writable and index was checked against LB_GETCOUNT.
    let rect_result = unsafe {
        SendMessageW(
            hwnd,
            LB_GETITEMRECT,
            index,
            &mut item_rect as *mut RECT as isize,
        )
    };
    if rect_result < 0
        || x < item_rect.left
        || x >= item_rect.right
        || y < item_rect.top
        || y >= item_rect.bottom
    {
        None
    } else {
        Some(index)
    }
}

fn selected_row() -> Option<usize> {
    // SAFETY: called only by main-thread window procedures.
    let app = unsafe { crate::app::try_app_mut() }?;
    let listbox = app.popup.controls.listbox;
    if listbox.is_null() {
        return None;
    }
    // SAFETY: LB_GETCURSEL only queries the live single-selection list.
    let selection = unsafe { SendMessageW(listbox, LB_GETCURSEL, 0, 0) };
    (selection >= 0).then_some(selection as usize)
}

fn focus_list() {
    // SAFETY: called only by the search procedure on the main thread.
    if let Some(app) = unsafe { crate::app::try_app_mut() } {
        if !app.popup.visible.is_empty() && !app.popup.controls.listbox.is_null() {
            // SAFETY: the visible list is a live child of the active popup.
            unsafe {
                SetFocus(app.popup.controls.listbox);
            }
        }
    }
}

fn execute_selected() {
    if let Some(index) = selected_row() {
        execute_index(index);
    }
}

/// Hide first, restore the original target, then queue normal snippet injection.
fn execute_index(list_index: usize) {
    // SAFETY: list/search procedures run on the main thread; shutdown is handled
    // by tolerating a missing App instance.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    let target = app.popup.target();
    let Some(snippet_index) = app.popup.item(list_index) else {
        return;
    };
    app.popup.hide();
    if !target.is_null() {
        // SAFETY: target is the valid injection window selected by App when the
        // picker opened.  SetForegroundWindow also tolerates a stale handle.
        unsafe {
            SetForegroundWindow(target);
        }
    }
    app.inject_snippet(snippet_index, target);
}

fn edit_selected() {
    if let Some(index) = selected_row() {
        edit_index(index);
    }
}

/// Hide before entering the future App editor API, as required by App's
/// main-thread/re-entrancy contract.  Editing never reactivates the inject target.
fn edit_index(list_index: usize) {
    // SAFETY: this command originates in a main-thread popup control procedure.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    let Some(snippet_index) = app.popup.item(list_index) else {
        return;
    };
    app.popup.hide();
    app.open_snippet_editor(Some(snippet_index));
}

fn open_new_snippet() {
    // SAFETY: this command originates in a main-thread button notification.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    app.popup.hide();
    app.open_snippet_editor(None);
}
