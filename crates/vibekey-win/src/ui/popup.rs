//! Compact native picker for snippets and text clipboard history.
//!
//! Both modes share one search box and an owner-drawn, single-line list. The
//! search-row menu replaces the old title/footer/buttons. Enter or one click
//! inserts the selected value; snippets additionally support F2/right-click
//! editing, while history supports Delete and a Clear button.

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
    EnableWindow, GetFocus, SetFocus, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_F2, VK_RETURN, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::ui::theme;
use crate::util::{self, wide};

const CLASS_NAME: &str = "VibekeyPopup";

const SEARCH_ID: i32 = 100;
const LISTBOX_ID: i32 = 101;
const EMPTY_ID: i32 = 103;
const MENU_BUTTON_ID: i32 = 110;
const CLEAR_BUTTON_ID: i32 = 112;

const WIDTH_LOGICAL: i32 = 460;
const MAX_HEIGHT_LOGICAL: i32 = 270;
const ITEM_HEIGHT_LOGICAL: i32 = 34;
const MAX_VISIBLE_ROWS: usize = 6;
const LIST_TOP_LOGICAL: i32 = 48;
const LIST_CHROME_LOGICAL: i32 = 56;
const PREVIEW_CHARS: usize = 120;
const POPUP_ALPHA: u8 = 242;

// Static-control styles from WinUser.h. Defining these locally avoids enabling
// another windows-sys feature solely for style constants.
const STATIC_CENTER: u32 = 0x0001;
const STATIC_CENTER_IMAGE: u32 = 0x0200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Snippets,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Snippet(usize),
    History(u64),
}

/// Detached display/search data. IDs, rather than row positions, identify the
/// value to execute so a clipboard update cannot turn a click into another item.
#[derive(Debug, Clone)]
pub struct PopupItem {
    kind: ItemKind,
    key: String,
    name: String,
    preview: String,
    search_text: String,
}

impl PopupItem {
    pub fn from_snippet(index: usize, snippet: &vibekey_core::Snippet) -> Self {
        let name = snippet.name.trim();
        let name = if name.is_empty() {
            "（无名称）".to_string()
        } else {
            name.to_string()
        };
        let key = snippet.key.trim().to_string();
        let preview = single_line(&snippet.text);
        let search_text = format!(
            "{} {} {} alt+{}",
            name.to_lowercase(),
            key.to_lowercase(),
            snippet.text.to_lowercase(),
            key.to_lowercase()
        );
        Self {
            kind: ItemKind::Snippet(index),
            key,
            name,
            preview,
            search_text,
        }
    }

    pub fn from_history(id: u64, text: &str) -> Self {
        Self {
            kind: ItemKind::History(id),
            key: String::new(),
            name: String::new(),
            preview: single_line(text),
            search_text: text.to_lowercase(),
        }
    }
}

/// Collapse all whitespace/newlines for one-row presentation. The original text
/// remains untouched in Config/ClipboardHistory and is what gets inserted.
fn single_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > PREVIEW_CHARS {
        let mut preview = collapsed
            .chars()
            .take(PREVIEW_CHARS - 1)
            .collect::<String>();
        preview.push('…');
        preview
    } else {
        collapsed
    }
}

#[derive(Default, Clone, Copy)]
struct Controls {
    search: HWND,
    listbox: HWND,
    empty: HWND,
    menu_button: HWND,
    clear_button: HWND,
}

/// Only one picker exists and it is owned by App on the main thread.
pub struct Popup {
    hwnd: HWND,
    controls: Controls,
    mode: PickerMode,
    items: Vec<PopupItem>,
    /// Indices into `items` for the currently visible rows.
    visible: Vec<usize>,
    target: HWND,
    open: bool,
    suppress_search_change: bool,
}

impl Popup {
    pub fn new() -> Self {
        Self {
            hwnd: std::ptr::null_mut(),
            controls: Controls::default(),
            mode: PickerMode::Snippets,
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

    fn visible_item(&self, index: usize) -> Option<&PopupItem> {
        let item_index = *self.visible.get(index)?;
        self.items.get(item_index)
    }

    fn snippet_index(&self, index: usize) -> Option<usize> {
        match self.visible_item(index)?.kind {
            ItemKind::Snippet(snippet_index) => Some(snippet_index),
            ItemKind::History(_) => None,
        }
    }

    /// Show either picker mode. An empty history is a valid state with its own
    /// empty label; an empty snippet list is handled by App before this call.
    pub fn show(
        &mut self,
        owner: HWND,
        mode: PickerMode,
        items: Vec<PopupItem>,
        target: HWND,
    ) -> bool {
        if let Err(error) = self.ensure_window(owner) {
            log::error!("could not create the picker popup: {error}");
            return false;
        }

        self.mode = mode;
        self.items = items;
        self.target = target;
        self.open = true;
        set_control_text(
            self.hwnd,
            match mode {
                PickerMode::Snippets => "Vibekey 片段选择器",
                PickerMode::History => "Vibekey 剪贴板历史",
            },
        );
        let cue = wide(match mode {
            PickerMode::Snippets => "搜索片段…",
            PickerMode::History => "搜索剪贴板历史…",
        });
        // SAFETY: the search is a live child and the cue buffer is copied by the
        // control synchronously. Clear exists only in history mode.
        unsafe {
            SendMessageW(
                self.controls.search,
                EM_SETCUEBANNER,
                1,
                cue.as_ptr() as isize,
            );
            ShowWindow(
                self.controls.clear_button,
                if mode == PickerMode::History {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
        }

        self.suppress_search_change = true;
        set_control_text(self.controls.search, "");
        self.suppress_search_change = false;
        self.apply_filter("");

        let anchor = util::caret_pos();
        let work = util::monitor_work_area(anchor);
        let width = util::scale(WIDTH_LOGICAL)
            .min((work.right - work.left - util::scale(8)).max(util::scale(1)));
        let rows = self.visible.len().clamp(1, MAX_VISIBLE_ROWS) as i32;
        let wanted_height =
            util::scale(LIST_CHROME_LOGICAL) + rows * util::scale(ITEM_HEIGHT_LOGICAL);
        let height = wanted_height
            .min(util::scale(MAX_HEIGHT_LOGICAL))
            .min((work.bottom - work.top - util::scale(8)).max(util::scale(1)));
        let position = util::clamp_to_monitor(anchor, width, height);

        // SAFETY: the popup handle is live and the physical-pixel rectangle has
        // been clamped to the nearest monitor's work area.
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

        if !util::force_foreground(self.hwnd) {
            log::warn!("could not take the foreground for the picker");
            // SAFETY: only the live popup's Z order is changed.
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
        // Resizing a previously empty/tiny ListBox can scroll its selection to
        // row one. Restore the intended initial view only after the final size
        // and focus have been applied, otherwise the first row stays clipped.
        // SAFETY: both controls are live children of the active popup.
        unsafe {
            SetFocus(self.controls.search);
            SendMessageW(self.controls.listbox, LB_SETTOPINDEX, 0, 0);
        }
        true
    }

    pub fn hide(&mut self) {
        self.open = false;
        if !self.hwnd.is_null() {
            // SAFETY: the live popup is only hidden, not destroyed.
            unsafe {
                ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    /// Remove one history value after Delete, retaining the user's query.
    fn remove_history(&mut self, id: u64) {
        self.items.retain(|item| item.kind != ItemKind::History(id));
        let query = control_text(self.controls.search);
        self.apply_filter(&query);
    }

    fn clear_history(&mut self) {
        self.items.clear();
        let query = control_text(self.controls.search);
        self.apply_filter(&query);
    }

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
        // SAFETY: the list is a live owner-drawn list box. LBS_HASSTRINGS copies
        // these empty labels; actual row text comes from visible_item.
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
            EnableWindow(
                self.controls.clear_button,
                (self.mode == PickerMode::History && !self.items.is_empty()) as i32,
            );
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
        }
        let empty_text = match (self.mode, self.items.is_empty()) {
            (PickerMode::Snippets, true) => "暂无可用片段",
            (PickerMode::Snippets, false) => "没有匹配的片段",
            (PickerMode::History, true) => "暂无剪贴板历史",
            (PickerMode::History, false) => "没有匹配的历史",
        };
        set_control_text(self.controls.empty, empty_text);
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
        // for the call. WS_POPUP deliberately has no system border.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
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
        if !util::set_window_alpha(hwnd, POPUP_ALPHA) {
            log::warn!("popup transparency is unavailable; using opaque drawing");
        }

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
        self.controls.menu_button = create_control(
            hwnd,
            instance,
            "BUTTON",
            "☰",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            MENU_BUTTON_ID,
        );
        self.controls.clear_button = create_control(
            hwnd,
            instance,
            "BUTTON",
            "清空",
            WS_CHILD | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            CLEAR_BUTTON_ID,
        );

        let controls = self.controls;
        if [
            controls.search,
            controls.listbox,
            controls.empty,
            controls.menu_button,
            controls.clear_button,
        ]
        .iter()
        .any(|handle| handle.is_null())
        {
            // SAFETY: creation failed; the hidden parent destroys any children.
            unsafe {
                DestroyWindow(hwnd);
            }
            self.hwnd = std::ptr::null_mut();
            self.controls = Controls::default();
            return Err("could not create one or more popup controls".to_string());
        }

        theme::set_font(controls.search, theme::body_font());
        theme::set_font(controls.empty, theme::body_font());
        theme::set_font(controls.menu_button, theme::semibold_font());
        theme::set_font(controls.clear_button, theme::small_font());

        // SAFETY: all child handles are live, shared theme fonts outlive them,
        // and subclass predecessors are retained for every subsequent message.
        unsafe {
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

/// Case-insensitive AND filtering over full text, not just the visible preview.
fn matches_query(item: &PopupItem, query: &str) -> bool {
    query
        .split_whitespace()
        .all(|term| item.search_text.contains(term))
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
    // live for the call, and id is encoded as the documented child HMENU.
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
    // SAFETY: the buffer has length + 1 writable UTF-16 code units and its
    // actual capacity is passed to GetWindowTextW.
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
}

fn hide_popup() {
    // SAFETY: window procedures run only on the main thread and tolerate the App
    // pointer having already been cleared during shutdown.
    if let Some(app) = unsafe { crate::app::try_app_mut() } {
        app.popup.hide();
    }
}

unsafe extern "system" fn popup_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ACTIVATE => {
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
            // SAFETY: Win32 supplies a valid structure for this message; the
            // null check also tolerates malformed synthetic messages.
            let measure = lparam as *mut MEASUREITEMSTRUCT;
            if !measure.is_null() && unsafe { (*measure).CtlID } == LISTBOX_ID as u32 {
                // SAFETY: the pointer was checked and is writable for this message.
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
            // SAFETY: client is writable and wparam is the supplied DC.
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
            hide_popup();
            0
        }
        WM_CLOSE => {
            hide_popup();
            0
        }
        _ => {
            // SAFETY: unhandled messages use the default procedure unchanged.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn handle_command(hwnd: HWND, wparam: WPARAM) {
    let id = (wparam as u32 & 0xFFFF) as i32;
    let notification = (wparam as u32 >> 16) & 0xFFFF;
    match (id, notification) {
        (SEARCH_ID, EN_CHANGE) => {
            // SAFETY: this main-thread notification tolerates shutdown's missing App.
            if let Some(app) = unsafe { crate::app::try_app_mut() } {
                if !app.popup.suppress_search_change {
                    let query = control_text(app.popup.controls.search);
                    app.popup.apply_filter(&query);
                }
            }
        }
        (SEARCH_ID, EN_SETFOCUS) | (SEARCH_ID, EN_KILLFOCUS) => {
            // SAFETY: invalidate the popup so the search frame repaints.
            unsafe {
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
        }
        (MENU_BUTTON_ID, BN_CLICKED) => open_quick_menu(),
        (CLEAR_BUTTON_ID, BN_CLICKED) => clear_history(),
        _ => {}
    }
}

struct SearchChrome {
    frame: RECT,
    menu: RECT,
    clear: RECT,
}

fn search_chrome(width: i32, history: bool) -> SearchChrome {
    let margin = util::scale(12);
    let gap = util::scale(8);
    let menu = util::rect(
        (width - margin - util::scale(30)).max(margin),
        util::scale(10),
        (width - margin).max(margin),
        util::scale(40),
    );
    let clear = if history {
        util::rect(
            (menu.left - gap - util::scale(44)).max(margin),
            menu.top,
            (menu.left - gap).max(margin),
            menu.bottom,
        )
    } else {
        util::rect(0, 0, 0, 0)
    };
    let right = if history { clear.left } else { menu.left } - gap;
    SearchChrome {
        frame: util::rect(margin, util::scale(9), right.max(margin), util::scale(41)),
        menu,
        clear,
    }
}

fn popup_is_history(hwnd: HWND) -> bool {
    // SAFETY: layout/paint run on the main thread; a missing App during shutdown
    // simply uses the snippet geometry for the final paint.
    unsafe { crate::app::try_app_mut() }
        .map(|app| app.popup.hwnd == hwnd && app.popup.mode == PickerMode::History)
        .unwrap_or(false)
}

fn layout_controls(hwnd: HWND, width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    let history = popup_is_history(hwnd);
    let chrome = search_chrome(width, history);
    let border = util::scale(1).max(1);
    let inset = util::scale(7);
    move_control(
        hwnd,
        SEARCH_ID,
        chrome.frame.left + inset,
        chrome.frame.top + util::scale(5),
        (chrome.frame.right - chrome.frame.left - 2 * inset).max(0),
        (chrome.frame.bottom - chrome.frame.top - util::scale(10)).max(0),
    );
    move_control(
        hwnd,
        MENU_BUTTON_ID,
        chrome.menu.left,
        chrome.menu.top,
        chrome.menu.right - chrome.menu.left,
        chrome.menu.bottom - chrome.menu.top,
    );
    if history {
        move_control(
            hwnd,
            CLEAR_BUTTON_ID,
            chrome.clear.left,
            chrome.clear.top,
            chrome.clear.right - chrome.clear.left,
            chrome.clear.bottom - chrome.clear.top,
        );
    }

    let margin = util::scale(12);
    let list_top = util::scale(LIST_TOP_LOGICAL);
    let list_height = (height - list_top - util::scale(8) - border).max(0);
    let content_width = (width - 2 * margin).max(0);
    move_control(
        hwnd,
        LISTBOX_ID,
        margin,
        list_top,
        content_width,
        list_height,
    );
    move_control(hwnd, EMPTY_ID, margin, list_top, content_width, list_height);
}

fn move_control(parent: HWND, id: i32, x: i32, y: i32, width: i32, height: i32) {
    // SAFETY: GetDlgItem only reads the live popup's child table.
    let control = unsafe { GetDlgItem(parent, id) };
    if control.is_null() {
        return;
    }
    // SAFETY: all dimensions are physical client coordinates.
    unsafe {
        MoveWindow(control, x, y, width, height, 1);
    }
}

fn paint_popup(hwnd: HWND) {
    // SAFETY: BeginPaint/EndPaint are paired and all drawing uses the returned DC.
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

        let chrome = search_chrome(client.right, popup_is_history(hwnd));
        let search = GetDlgItem(hwnd, SEARCH_ID);
        let focused = !search.is_null() && GetFocus() == search;
        theme::fill_rounded(
            hdc,
            &chrome.frame,
            util::scale(8),
            if focused {
                theme::ACCENT
            } else {
                theme::BORDER
            },
        );
        let border = util::scale(1).max(1);
        let inner = util::rect(
            chrome.frame.left + border,
            chrome.frame.top + border,
            chrome.frame.right - border,
            chrome.frame.bottom - border,
        );
        theme::fill_rounded(hdc, &inner, util::scale(7), theme::SURFACE);

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
    // Clone the display model so no App borrow remains live across GDI calls.
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

    let mut text_left = row.left + util::scale(12);
    if let ItemKind::Snippet(_) = item.kind {
        let chip = util::rect(
            row.left + util::scale(6),
            row.top + util::scale(7),
            row.left + util::scale(62),
            row.top + util::scale(27),
        );
        theme::fill_rounded(
            draw.hDC,
            &chip,
            util::scale(5),
            if selected {
                theme::ACCENT
            } else {
                theme::ACCENT_SOFT
            },
        );
        let shortcut = if item.key.is_empty() {
            "—".to_string()
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
        text_left = row.left + util::scale(72);
    }

    let text_rect = util::rect(
        text_left,
        row.top + util::scale(2),
        row.right - util::scale(10),
        row.bottom - util::scale(2),
    );
    if text_rect.right <= text_rect.left {
        return;
    }
    let value = match item.kind {
        ItemKind::Snippet(_) if item.preview.is_empty() => item.name,
        ItemKind::Snippet(_) => format!("{}  ·  {}", item.name, item.preview),
        ItemKind::History(_) if item.preview.is_empty() => "（空白文本）".to_string(),
        ItemKind::History(_) => item.preview,
    };
    theme::text(
        draw.hDC,
        &text_rect,
        &value,
        theme::body_font(),
        theme::TEXT,
        true,
    );
}

fn draw_centred_text(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    value: &str,
    color: windows_sys::Win32::Foundation::COLORREF,
) {
    let mut draw_rect = *rect;
    let value = wide(value);
    // SAFETY: the DC/rectangle belong to the active draw operation, the UTF-16
    // buffer is NUL terminated, and the shared font is process-lived.
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
            VK_DELETE => {
                delete_selected();
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
    // SAFETY: the atomic stores the original LISTBOX WNDPROC installed by Windows.
    let previous: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: previous is the validated system procedure described above.
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
    // SAFETY: previous is called with unchanged arguments.
    unsafe { CallWindowProcW(previous, hwnd, message, wparam, lparam) }
}

/// Resolve a click without accepting the nearest row below the final item.
fn clicked_index(hwnd: HWND, lparam: LPARAM) -> Option<usize> {
    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    let packed = (((y as u16 as u32) << 16) | (x as u16 as u32)) as isize;
    // SAFETY: LB_ITEMFROMPOINT reads the live list with signed client coordinates.
    let result = unsafe { SendMessageW(hwnd, LB_ITEMFROMPOINT, 0, packed) };
    if ((result >> 16) & 0xFFFF) != 0 {
        return None;
    }
    let index = (result & 0xFFFF) as usize;
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

fn execute_index(list_index: usize) {
    // SAFETY: list/search procedures run on the main thread.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    let target = app.popup.target();
    let Some(kind) = app.popup.visible_item(list_index).map(|item| item.kind) else {
        return;
    };
    app.popup.hide();
    if !target.is_null() {
        // SAFETY: target was selected by App and SetForegroundWindow tolerates a
        // stale handle; injection revalidates it on the worker.
        unsafe {
            SetForegroundWindow(target);
        }
    }
    match kind {
        ItemKind::Snippet(index) => app.inject_snippet(index, target),
        ItemKind::History(id) => app.inject_history(id, target),
    }
}

fn edit_selected() {
    if let Some(index) = selected_row() {
        edit_index(index);
    }
}

fn edit_index(list_index: usize) {
    // SAFETY: this command originates in a main-thread popup procedure.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    // History rows deliberately have no snippet edit action.
    let Some(snippet_index) = app.popup.snippet_index(list_index) else {
        return;
    };
    app.popup.hide();
    app.open_snippet_editor(Some(snippet_index));
}

fn delete_selected() {
    // Resolve the row before borrowing App again: selected_row itself performs a
    // short try_app_mut access and must not be nested inside another App borrow.
    let Some(row) = selected_row() else {
        return;
    };
    // SAFETY: this command originates in a main-thread list procedure.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    let Some(ItemKind::History(id)) = app.popup.visible_item(row).map(|item| item.kind) else {
        return;
    };
    if app.history.remove(id) {
        app.popup.remove_history(id);
    }
}

fn clear_history() {
    // SAFETY: this command originates in a main-thread button notification.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    app.history.clear();
    app.popup.clear_history();
}

fn open_quick_menu() {
    // SAFETY: this command originates in a main-thread button notification.
    let Some(app) = (unsafe { crate::app::try_app_mut() }) else {
        return;
    };
    let mut rect = util::rect(0, 0, 0, 0);
    // SAFETY: rect is writable and the menu button is a live child window.
    unsafe {
        GetWindowRect(app.popup.controls.menu_button, &mut rect);
    }
    app.popup.hide();
    app.queue_menu(windows_sys::Win32::Foundation::POINT {
        x: rect.left,
        y: rect.bottom,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_single_line_and_preserve_searchable_original() {
        let snippet = vibekey_core::Snippet::new("1", "问候", "第一行\n第二行\t结束");
        let item = PopupItem::from_snippet(3, &snippet);
        assert_eq!(item.preview, "第一行 第二行 结束");
        assert!(matches_query(&item, "第二行"));
        assert!(matches_query(&item, "alt+1 问候"));
        assert!(!matches_query(&item, "缺失"));
    }

    #[test]
    fn history_search_uses_text_beyond_the_visible_preview() {
        let text = format!("{}needle", "x".repeat(PREVIEW_CHARS + 20));
        let item = PopupItem::from_history(7, &text);
        assert!(item.preview.chars().count() <= PREVIEW_CHARS);
        assert!(matches_query(&item, "needle"));
        assert_eq!(item.kind, ItemKind::History(7));
    }
}
