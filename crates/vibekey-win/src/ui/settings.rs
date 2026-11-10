//! Vibekey's settings and snippet management window.
//!
//! The window is built entirely with native Win32 controls.  Three pages keep
//! the interface approachable: snippets, input/hotkeys and synchronisation.
//! Snippet edits update an in-memory Config immediately; Apply/Save performs
//! one validated atomic write, so Cancel never leaves partial TOML edits.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use vibekey_core::crypto::{decrypt_config, validate_password, CryptoError};
use vibekey_core::keymap::SLOT_KEYS;
use vibekey_core::sync::{CertificateFingerprint, UreqWebDav, WebDav};
use vibekey_core::{ActivationMode, Config, InputMethod, Keymap, SecretKind, SecretStore, Snippet};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, DrawFocusRect, EndPaint, InvalidateRect, SetBkColor, SetBkMode, SetTextColor,
    UpdateWindow, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_LIMITTEXT, EM_REPLACESEL, EM_SETCUEBANNER, EM_SETSEL, MEASUREITEMSTRUCT,
    ODS_SELECTED,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::autostart;
use crate::ui::theme;
use crate::util::{self, wide, WM_APP_SHOW_SNIPPET, WM_APP_TEST_RESULT};

const CLASS_NAME: &str = "VibekeySettings";
const PAGE_CLASS_NAME: &str = "VibekeySettingsPage";
const CLIENT_WIDTH: i32 = 940;
const CLIENT_HEIGHT: i32 = 630;
const SIDEBAR_WIDTH: i32 = 204;
const PAGE_SNIPPETS: usize = 0;
const PAGE_BEHAVIOR: usize = 1;
const PAGE_SYNC: usize = 2;
const STATIC_NO_WRAP: u32 = 0x0000_000C;
const STATIC_END_ELLIPSIS: u32 = 0x0000_4000;

const ID_NAV_SNIPPETS: i32 = 200;
const ID_NAV_BEHAVIOR: i32 = 201;
const ID_NAV_SYNC: i32 = 202;
const ID_HEADER_SUBTITLE: i32 = 203;
const ID_STATUS: i32 = 204;

const ID_SNIPPET_SEARCH: i32 = 300;
const ID_SNIPPET_LIST: i32 = 301;
const ID_SNIPPET_NEW: i32 = 302;
const ID_SNIPPET_DUPLICATE: i32 = 303;
const ID_SNIPPET_DELETE: i32 = 304;
const ID_SNIPPET_NAME: i32 = 305;
const ID_SNIPPET_KEY: i32 = 306;
const ID_SNIPPET_TEXT: i32 = 307;
const ID_SNIPPET_SEND_ENTER: i32 = 308;
const ID_SNIPPET_ENABLED: i32 = 309;
const ID_SNIPPET_VARIABLE: i32 = 310;
const ID_SNIPPET_INSERT_VARIABLE: i32 = 311;

const ID_SYNC_ENABLED: i32 = 400;
const ID_URL: i32 = 401;
const ID_USER: i32 = 402;
const ID_PASSWORD: i32 = 403;
const ID_INTERVAL: i32 = 404;
const ID_SYNC_ON_CHANGE: i32 = 405;
const ID_BACKUP: i32 = 406;
const ID_TEST: i32 = 407;
const ID_CLEAR_PASSWORD: i32 = 408;
const ID_TIMEOUT: i32 = 409;
const ID_PASSWORD_STATE: i32 = 410;
const ID_ENCRYPT: i32 = 411;
const ID_ENCRYPTION_PASSWORD: i32 = 412;
const ID_CLEAR_ENCRYPTION_PASSWORD: i32 = 413;
const ID_ENCRYPTION_PASSWORD_STATE: i32 = 414;
const ID_CERTIFICATE_SHA256: i32 = 415;

const ID_INPUT_METHOD: i32 = 500;
const ID_ACTIVATION: i32 = 501;
const ID_AUTOSTART: i32 = 502;
const ID_HOLD_MS: i32 = 503;
const ID_RESTORE_CLIPBOARD: i32 = 504;
const ID_LIST_HOTKEY: i32 = 505;
const ID_CONFIG_HOTKEY: i32 = 506;
const ID_IGNORE_APPS: i32 = 507;
const ID_HISTORY_HOTKEY: i32 = 508;

const ID_OPEN_CONFIG: i32 = 600;
const ID_APPLY: i32 = 601;
const ID_SAVE: i32 = IDOK;
const ID_CANCEL: i32 = IDCANCEL;

const VARIABLES: [(&str, &str); 6] = [
    ("{date}", "日期 · {date}"),
    ("{time}", "时间 · {time}"),
    ("{datetime}", "日期时间 · {datetime}"),
    ("{clipboard}", "剪贴板 · {clipboard}"),
    ("{user}", "Windows 用户 · {user}"),
    ("{host}", "计算机名 · {host}"),
];

#[derive(Default, Clone, Copy)]
struct SnippetControls {
    search: HWND,
    count: HWND,
    list: HWND,
    new: HWND,
    duplicate: HWND,
    editor_heading: HWND,
    name_label: HWND,
    name: HWND,
    key_label: HWND,
    key: HWND,
    text_label: HWND,
    text: HWND,
    send_enter: HWND,
    enabled: HWND,
    variable_label: HWND,
    variable: HWND,
    insert_variable: HWND,
    delete: HWND,
    editor_hint: HWND,
}

#[derive(Default, Clone, Copy)]
struct BehaviorControls {
    input_heading: HWND,
    input_method_label: HWND,
    input_method: HWND,
    restore_clipboard: HWND,
    activation_heading: HWND,
    activation_label: HWND,
    activation: HWND,
    hold_label: HWND,
    hold_ms: HWND,
    hold_hint: HWND,
    autostart: HWND,
    hotkeys_heading: HWND,
    list_hotkey_label: HWND,
    list_hotkey: HWND,
    config_hotkey_label: HWND,
    config_hotkey: HWND,
    history_hotkey_label: HWND,
    history_hotkey: HWND,
    ignore_label: HWND,
    ignore_apps: HWND,
    ignore_hint: HWND,
}

#[derive(Default, Clone, Copy)]
struct SyncControls {
    enabled: HWND,
    url_label: HWND,
    url: HWND,
    user_label: HWND,
    user: HWND,
    password_label: HWND,
    password: HWND,
    password_state: HWND,
    clear_password: HWND,
    encrypt: HWND,
    encryption_password_label: HWND,
    encryption_password: HWND,
    encryption_password_state: HWND,
    clear_encryption_password: HWND,
    interval_label: HWND,
    interval: HWND,
    timeout_label: HWND,
    timeout: HWND,
    sync_on_change: HWND,
    backup: HWND,
    test: HWND,
    certificate_fingerprint: HWND,
}

struct SettingsState {
    hwnd: HWND,
    pages: [HWND; 3],
    nav: [HWND; 3],
    header_title: HWND,
    header_subtitle: HWND,
    status: HWND,
    open_config: HWND,
    apply: HWND,
    save: HWND,
    cancel: HWND,
    snippets: SnippetControls,
    behavior: BehaviorControls,
    sync: SyncControls,
    current_page: usize,
    config: Config,
    config_path: PathBuf,
    original_mtime: Option<SystemTime>,
    secrets: Arc<dyn SecretStore>,
    main_hwnd: HWND,
    test_result: Arc<Mutex<Option<(bool, String)>>>,
    test_running: bool,
    clear_password_requested: bool,
    clear_encryption_password_requested: bool,
    selected_snippet: Option<usize>,
    snippet_indices: Vec<usize>,
    snippet_filter: String,
    loading: bool,
    dirty: bool,
}

static REGISTERED: AtomicBool = AtomicBool::new(false);

/// Open the settings window.  The app still owns the singleton window handle.
pub fn open(
    main_hwnd: HWND,
    config: &Config,
    config_path: &Path,
    secrets: Arc<dyn SecretStore>,
) -> Option<HWND> {
    if !ensure_classes() {
        log::error!("could not register the settings window classes");
        return None;
    }
    let style = WS_OVERLAPPEDWINDOW;
    let mut rect = util::rect(0, 0, util::scale(CLIENT_WIDTH), util::scale(CLIENT_HEIGHT));
    // SAFETY: rect is a valid RECT adjusted in place for the selected style.
    unsafe {
        AdjustWindowRect(&mut rect, style, 0);
    }
    // SAFETY: GetSystemMetrics has no preconditions.
    let (screen_width, screen_height) =
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let width = (rect.right - rect.left).min((screen_width * 94 / 100).max(util::scale(640)));
    let height = (rect.bottom - rect.top).min((screen_height * 92 / 100).max(util::scale(480)));
    let x = ((screen_width - width) / 2).max(0);
    let y = ((screen_height - height) / 2).max(0);

    let class_name = wide(CLASS_NAME);
    let title = wide("Vibekey · 设置与片段管理");
    // SAFETY: the class is registered and the string buffers live for each call.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            width,
            height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        log::error!("could not create the settings window");
        return None;
    }

    let state = Box::new(SettingsState {
        hwnd,
        pages: [std::ptr::null_mut(); 3],
        nav: [std::ptr::null_mut(); 3],
        header_title: std::ptr::null_mut(),
        header_subtitle: std::ptr::null_mut(),
        status: std::ptr::null_mut(),
        open_config: std::ptr::null_mut(),
        apply: std::ptr::null_mut(),
        save: std::ptr::null_mut(),
        cancel: std::ptr::null_mut(),
        snippets: SnippetControls::default(),
        behavior: BehaviorControls::default(),
        sync: SyncControls::default(),
        current_page: PAGE_SNIPPETS,
        config: config.clone(),
        config_path: config_path.to_path_buf(),
        original_mtime: file_mtime(config_path),
        secrets,
        main_hwnd,
        test_result: Arc::new(Mutex::new(None)),
        test_running: false,
        clear_password_requested: false,
        clear_encryption_password_requested: false,
        selected_snippet: None,
        snippet_indices: Vec::new(),
        snippet_filter: String::new(),
        loading: true,
        dirty: false,
    });
    let pointer = Box::into_raw(state);
    // SAFETY: the pointer is owned by the window until WM_NCDESTROY.  Controls
    // are created before the window becomes visible, so no partial state is seen.
    unsafe {
        util::set_window_userdata(hwnd, pointer as isize);
        create_controls(&mut *pointer);
        refresh_snippet_list(&mut *pointer, Some(0));
        select_page(&mut *pointer, PAGE_SNIPPETS);
        layout(&mut *pointer);
        (*pointer).loading = false;
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        util::force_foreground(hwnd);
        if !(*pointer).snippets.list.is_null() {
            SetFocus((*pointer).snippets.list);
        }
    }
    Some(hwnd)
}

/// Ask an existing settings window to create or edit a snippet.
/// `None` means a new snippet. For an existing item, `expected` is the App
/// snapshot's value and resolves identity when the editor's working order has
/// changed. The reference only needs to live during this synchronous call.
pub fn show_snippet_editor(hwnd: HWND, index: Option<usize>, expected: Option<&Snippet>) {
    let wparam = index.map(|value| value + 1).unwrap_or(0);
    let lparam = expected
        .map(|snippet| snippet as *const Snippet as isize)
        .unwrap_or(0);
    // SAFETY: the app verifies that this is a live settings window. SendMessage
    // is synchronous and `expected` remains alive until this function returns.
    unsafe {
        SendMessageW(hwnd, WM_APP_SHOW_SNIPPET, wparam, lparam);
    }
}

fn ensure_classes() -> bool {
    if REGISTERED.load(Ordering::Acquire) {
        return true;
    }
    // SAFETY: class-name buffers live for registration; Windows copies them.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let page_name = wide(PAGE_CLASS_NAME);
        let mut page: WNDCLASSW = std::mem::zeroed();
        page.lpfnWndProc = Some(page_proc);
        page.hInstance = instance;
        page.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        page.hbrBackground = theme::surface_brush();
        page.lpszClassName = page_name.as_ptr();
        if RegisterClassW(&page) == 0 {
            return false;
        }

        let class_name = wide(CLASS_NAME);
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(settings_proc);
        class.hInstance = instance;
        class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        class.hbrBackground = theme::background_brush();
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return false;
        }
    }
    REGISTERED.store(true, Ordering::Release);
    true
}

/// Create a native child control with a shared Vibekey font.
unsafe fn create_control(
    parent: HWND,
    class_name: &str,
    text: &str,
    ex_style: u32,
    style: u32,
    id: i32,
    font: windows_sys::Win32::Graphics::Gdi::HFONT,
) -> HWND {
    let class_wide = wide(class_name);
    let text_wide = wide(text);
    let instance = GetModuleHandleW(std::ptr::null());
    let hwnd = CreateWindowExW(
        ex_style,
        class_wide.as_ptr(),
        text_wide.as_ptr(),
        style | WS_CHILD | WS_VISIBLE,
        0,
        0,
        10,
        10,
        parent,
        id as usize as *mut core::ffi::c_void,
        instance,
        std::ptr::null(),
    );
    theme::set_font(hwnd, font);
    hwnd
}

unsafe fn small_label(parent: HWND, text: &str, id: i32) -> HWND {
    create_control(parent, "STATIC", text, 0, 0, id, theme::small_font())
}

unsafe fn heading(parent: HWND, text: &str) -> HWND {
    create_control(parent, "STATIC", text, 0, 0, -1, theme::semibold_font())
}

unsafe fn edit(parent: HWND, text: &str, id: i32, password: bool) -> HWND {
    let mut style = ES_LEFT | ES_AUTOHSCROLL;
    if password {
        style |= ES_PASSWORD;
    }
    let hwnd = create_control(
        parent,
        "EDIT",
        text,
        0,
        WS_BORDER | WS_TABSTOP | style as u32,
        id,
        theme::body_font(),
    );
    // Limit ordinary single-line fields without restricting snippet text.
    if id != ID_SNIPPET_TEXT && id != ID_IGNORE_APPS {
        SendMessageW(hwnd, EM_LIMITTEXT, 4096, 0);
    }
    hwnd
}

unsafe fn multiline_edit(parent: HWND, text: &str, id: i32) -> HWND {
    create_control(
        parent,
        "EDIT",
        text,
        0,
        WS_BORDER
            | WS_VSCROLL
            | WS_TABSTOP
            | (ES_LEFT | ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32,
        id,
        theme::body_font(),
    )
}

unsafe fn button(parent: HWND, text: &str, id: i32, primary: bool) -> HWND {
    create_control(
        parent,
        "BUTTON",
        text,
        0,
        if primary {
            BS_OWNERDRAW as u32
        } else {
            BS_PUSHBUTTON as u32
        } | WS_TABSTOP,
        id,
        theme::body_font(),
    )
}

unsafe fn checkbox(parent: HWND, text: &str, checked: bool, id: i32) -> HWND {
    let control = create_control(
        parent,
        "BUTTON",
        text,
        0,
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        id,
        theme::body_font(),
    );
    if checked {
        SendMessageW(control, BM_SETCHECK, 1, 0);
    }
    control
}

unsafe fn combo(parent: HWND, id: i32, values: &[&str], selected: usize) -> HWND {
    let control = create_control(
        parent,
        "COMBOBOX",
        "",
        0,
        CBS_DROPDOWNLIST as u32 | WS_VSCROLL | WS_TABSTOP,
        id,
        theme::body_font(),
    );
    for value in values {
        let value = wide(value);
        SendMessageW(control, CB_ADDSTRING, 0, value.as_ptr() as isize);
    }
    SendMessageW(control, CB_SETCURSEL, selected, 0);
    control
}

unsafe fn cue_banner(hwnd: HWND, text: &str) {
    let text = wide(text);
    SendMessageW(hwnd, EM_SETCUEBANNER, 1, text.as_ptr() as isize);
}

unsafe fn create_controls(state: &mut SettingsState) {
    let hwnd = state.hwnd;
    state.header_title = create_control(hwnd, "STATIC", "片段库", 0, 0, -1, theme::title_font());
    state.header_subtitle = small_label(hwnd, "", ID_HEADER_SUBTITLE);
    state.status = create_control(
        hwnd,
        "STATIC",
        "所有更改将在应用后生效",
        0,
        STATIC_NO_WRAP | STATIC_END_ELLIPSIS,
        ID_STATUS,
        theme::small_font(),
    );
    state.nav = [
        button(hwnd, "片段库", ID_NAV_SNIPPETS, true),
        button(hwnd, "输入与快捷键", ID_NAV_BEHAVIOR, true),
        button(hwnd, "同步与备份", ID_NAV_SYNC, true),
    ];
    state.open_config = button(hwnd, "打开 TOML", ID_OPEN_CONFIG, false);
    state.apply = button(hwnd, "应用", ID_APPLY, false);
    state.cancel = button(hwnd, "取消", ID_CANCEL, false);
    state.save = button(hwnd, "保存并关闭", ID_SAVE, true);

    let instance = GetModuleHandleW(std::ptr::null());
    let page_class = wide(PAGE_CLASS_NAME);
    let empty = wide("");
    for page in &mut state.pages {
        *page = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            page_class.as_ptr(),
            empty.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
            0,
            0,
            100,
            100,
            hwnd,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
    }

    create_snippet_controls(state);
    create_behavior_controls(state);
    create_sync_controls(state);
    update_password_state(state);
}

unsafe fn create_snippet_controls(state: &mut SettingsState) {
    let page = state.pages[PAGE_SNIPPETS];
    let c = &mut state.snippets;
    c.search = edit(page, "", ID_SNIPPET_SEARCH, false);
    cue_banner(c.search, "搜索名称、快捷键或内容…");
    c.count = small_label(page, "", -1);
    c.list = create_control(
        page,
        "LISTBOX",
        "",
        0,
        WS_BORDER
            | WS_VSCROLL
            | WS_TABSTOP
            | (LBS_NOTIFY | LBS_OWNERDRAWFIXED | LBS_HASSTRINGS | LBS_NOINTEGRALHEIGHT) as u32,
        ID_SNIPPET_LIST,
        theme::body_font(),
    );
    c.new = button(page, "＋ 新建", ID_SNIPPET_NEW, false);
    c.duplicate = button(page, "复制", ID_SNIPPET_DUPLICATE, false);
    c.editor_heading = heading(page, "快速编辑");
    c.name_label = small_label(page, "片段名称", -1);
    c.name = edit(page, "", ID_SNIPPET_NAME, false);
    cue_banner(c.name, "例如：工作邮箱");
    c.key_label = small_label(page, "Alt 快捷键", -1);
    let mut keys = vec!["不绑定（仅列表）"];
    keys.extend(SLOT_KEYS.iter().map(|key| match *key {
        "-" => "Alt + -",
        "=" => "Alt + =",
        key => match key {
            "1" => "Alt + 1",
            "2" => "Alt + 2",
            "3" => "Alt + 3",
            "4" => "Alt + 4",
            "5" => "Alt + 5",
            "6" => "Alt + 6",
            "7" => "Alt + 7",
            "8" => "Alt + 8",
            "9" => "Alt + 9",
            "0" => "Alt + 0",
            _ => "Alt + ?",
        },
    }));
    c.key = combo(page, ID_SNIPPET_KEY, &keys, 0);
    c.text_label = small_label(page, "插入内容（支持多行与模板变量）", -1);
    c.text = multiline_edit(page, "", ID_SNIPPET_TEXT);
    c.send_enter = checkbox(page, "插入后按 Enter", false, ID_SNIPPET_SEND_ENTER);
    c.enabled = checkbox(page, "启用此片段", true, ID_SNIPPET_ENABLED);
    c.variable_label = small_label(page, "模板变量", -1);
    let variable_names: Vec<&str> = VARIABLES.iter().map(|(_, label)| *label).collect();
    c.variable = combo(page, ID_SNIPPET_VARIABLE, &variable_names, 0);
    c.insert_variable = button(page, "插入变量", ID_SNIPPET_INSERT_VARIABLE, false);
    c.delete = button(page, "删除片段", ID_SNIPPET_DELETE, false);
    c.editor_hint = small_label(page, "修改会立即暂存；点击“应用”写入配置。", -1);
}

unsafe fn create_behavior_controls(state: &mut SettingsState) {
    let page = state.pages[PAGE_BEHAVIOR];
    let c = &mut state.behavior;
    let config = &state.config;
    c.input_heading = heading(page, "输入体验");
    c.input_method_label = small_label(page, "文字输入方式", -1);
    c.input_method = combo(
        page,
        ID_INPUT_METHOD,
        &[
            "剪贴板粘贴（推荐，中文最稳）",
            "直接输入（SendInput Unicode）",
        ],
        match config.input_method {
            InputMethod::Paste => 0,
            InputMethod::Type => 1,
        },
    );
    c.restore_clipboard = checkbox(
        page,
        "输入后恢复纯文本剪贴板",
        config.restore_clipboard,
        ID_RESTORE_CLIPBOARD,
    );
    c.activation_heading = heading(page, "触发方式");
    c.activation_label = small_label(page, "Alt + 数字的响应时机", -1);
    c.activation = combo(
        page,
        ID_ACTIVATION,
        &["按下立即触发", "按住 Alt 一段时间后触发"],
        match config.activation {
            ActivationMode::Immediate => 0,
            ActivationMode::Hold => 1,
        },
    );
    c.hold_label = small_label(page, "按住时长", -1);
    c.hold_ms = edit(page, &config.hold_ms.to_string(), ID_HOLD_MS, false);
    c.hold_hint = small_label(page, "毫秒（建议 100–3000）", -1);
    c.autostart = checkbox(
        page,
        "登录 Windows 后自动启动",
        config.start_with_windows,
        ID_AUTOSTART,
    );

    c.hotkeys_heading = heading(page, "全局快捷键");
    c.list_hotkey_label = small_label(page, "打开片段列表", -1);
    c.list_hotkey = edit(page, &config.hotkeys.list, ID_LIST_HOTKEY, false);
    theme::set_font(c.list_hotkey, theme::mono_font());
    c.config_hotkey_label = small_label(page, "打开配置菜单", -1);
    c.config_hotkey = edit(page, &config.hotkeys.config, ID_CONFIG_HOTKEY, false);
    theme::set_font(c.config_hotkey, theme::mono_font());
    c.history_hotkey_label = small_label(page, "打开剪贴板历史", -1);
    c.history_hotkey = edit(page, &config.hotkeys.history, ID_HISTORY_HOTKEY, false);
    theme::set_font(c.history_hotkey, theme::mono_font());
    c.ignore_label = small_label(page, "忽略程序（每行一个 exe 名称）", -1);
    c.ignore_apps = multiline_edit(page, &config.ignore_apps.join("\r\n"), ID_IGNORE_APPS);
    c.ignore_hint = small_label(
        page,
        "例如 notepad.exe。热键示例：alt+`、alt+backspace、ctrl+shift+h。",
        -1,
    );
}

unsafe fn create_sync_controls(state: &mut SettingsState) {
    let page = state.pages[PAGE_SYNC];
    let c = &mut state.sync;
    let config = &state.config.sync;
    c.enabled = checkbox(
        page,
        "启用 WebDAV 同步，在多台设备间共享片段",
        config.enabled,
        ID_SYNC_ENABLED,
    );
    c.url_label = small_label(page, "WebDAV 配置文件地址", -1);
    c.url = edit(page, &config.url, ID_URL, false);
    cue_banner(c.url, "https://dav.example.com/vibekey/config.toml");
    c.user_label = small_label(page, "用户名", -1);
    c.user = edit(page, &config.username, ID_USER, false);
    c.password_label = small_label(page, "密码 / 应用密码", -1);
    c.password = edit(page, "", ID_PASSWORD, true);
    cue_banner(c.password, "留空则保留已保存密码");
    c.password_state = small_label(page, "", ID_PASSWORD_STATE);
    c.clear_password = button(page, "清除登录密码", ID_CLEAR_PASSWORD, false);
    c.encrypt = checkbox(
        page,
        "加密远端配置正文（HTTP 下也保护片段内容）",
        config.encrypt,
        ID_ENCRYPT,
    );
    c.encryption_password_label = small_label(page, "独立同步加密密码（至少 8 个字符）", -1);
    c.encryption_password = edit(page, "", ID_ENCRYPTION_PASSWORD, true);
    cue_banner(c.encryption_password, "与登录密码分开；留空保留已存密码");
    c.encryption_password_state = small_label(page, "", ID_ENCRYPTION_PASSWORD_STATE);
    c.clear_encryption_password = button(page, "清除加密密码", ID_CLEAR_ENCRYPTION_PASSWORD, false);
    c.interval_label = small_label(page, "同步间隔（分钟，0 = 不定时）", -1);
    c.interval = edit(
        page,
        &config.interval_minutes.to_string(),
        ID_INTERVAL,
        false,
    );
    c.timeout_label = small_label(page, "连接超时（秒）", -1);
    c.timeout = edit(page, &config.timeout_secs.to_string(), ID_TIMEOUT, false);
    c.sync_on_change = checkbox(
        page,
        "保存配置后立即同步",
        config.sync_on_change,
        ID_SYNC_ON_CHANGE,
    );
    c.backup = checkbox(
        page,
        "发生冲突时先备份远端配置",
        config.backup_on_conflict,
        ID_BACKUP,
    );
    c.test = button(page, "测试连接", ID_TEST, false);
    c.certificate_fingerprint = edit(
        page,
        &config.certificate_sha256,
        ID_CERTIFICATE_SHA256,
        false,
    );
    cue_banner(
        c.certificate_fingerprint,
        "可选：自签名证书 SHA-256 指纹（64 位十六进制）",
    );
    theme::set_font(c.certificate_fingerprint, theme::mono_font());
}

fn window_text(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return String::new();
    }
    // First query the actual UTF-16 length, then allocate.  This avoids cutting
    // off long multiline snippets at the old fixed 4096-unit buffer boundary.
    // SAFETY: the handle is a live edit control.
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    // SAFETY: the allocated buffer has the exact length reported by the control.
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        String::new()
    } else {
        util::from_wide(&buffer[..copied as usize])
    }
}

fn set_text(hwnd: HWND, text: &str) {
    if hwnd.is_null() {
        return;
    }
    let text = wide(text);
    // SAFETY: the handle is a live control and the buffer is NUL terminated.
    unsafe {
        SetWindowTextW(hwnd, text.as_ptr());
    }
}

fn checked(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    // SAFETY: the handle is a live check box; BM_GETCHECK only reads its state.
    unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) == 1 }
}

fn combo_selection(hwnd: HWND) -> usize {
    if hwnd.is_null() {
        return 0;
    }
    // SAFETY: the handle is a live combo box.
    let index = unsafe { SendMessageW(hwnd, CB_GETCURSEL, 0, 0) };
    if index < 0 {
        0
    } else {
        index as usize
    }
}

fn set_enabled(hwnd: HWND, enabled: bool) {
    if !hwnd.is_null() {
        // SAFETY: the handle is a live child control.
        unsafe {
            EnableWindow(hwnd, if enabled { 1 } else { 0 });
        }
    }
}

fn move_window(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) {
    if hwnd.is_null() {
        return;
    }
    // SAFETY: all handles belong to this window tree; NOACTIVATE prevents a
    // layout pass from unexpectedly changing keyboard focus.
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            width.max(0),
            height.max(0),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn layout(state: &mut SettingsState) {
    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: client is a valid out parameter and hwnd is live.
    unsafe {
        GetClientRect(state.hwnd, &mut client);
    }
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    let side = util::scale(SIDEBAR_WIDTH);
    let margin = util::scale(24);
    let content_x = side + margin;
    let content_width = (width - content_x - margin).max(util::scale(320));

    move_window(
        state.header_title,
        content_x,
        util::scale(18),
        content_width,
        util::scale(34),
    );
    move_window(
        state.header_subtitle,
        content_x + util::scale(2),
        util::scale(54),
        content_width,
        util::scale(22),
    );
    for (index, nav) in state.nav.iter().enumerate() {
        move_window(
            *nav,
            util::scale(14),
            util::scale(112 + index as i32 * 52),
            side - util::scale(28),
            util::scale(42),
        );
    }

    let page_y = util::scale(88);
    let footer_height = util::scale(66);
    let page_height = (height - page_y - footer_height - util::scale(12)).max(util::scale(280));
    for page in state.pages {
        move_window(
            page,
            side + util::scale(16),
            page_y,
            width - side - util::scale(32),
            page_height,
        );
    }

    let button_y = height - util::scale(46);
    move_window(
        state.open_config,
        content_x,
        button_y,
        util::scale(100),
        util::scale(30),
    );
    move_window(
        state.status,
        content_x + util::scale(112),
        button_y + util::scale(3),
        (content_width - util::scale(112 + 302)).max(util::scale(80)),
        util::scale(24),
    );
    move_window(
        state.apply,
        width - margin - util::scale(290),
        button_y,
        util::scale(76),
        util::scale(32),
    );
    move_window(
        state.cancel,
        width - margin - util::scale(204),
        button_y,
        util::scale(76),
        util::scale(32),
    );
    move_window(
        state.save,
        width - margin - util::scale(118),
        button_y,
        util::scale(118),
        util::scale(32),
    );

    layout_snippets(state, page_height);
    layout_behavior(state, page_height);
    layout_sync(state, page_height);
}

fn page_width(state: &SettingsState, page: usize) -> i32 {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: rect is a valid out parameter and the page is a live child.
    unsafe {
        GetClientRect(state.pages[page], &mut rect);
    }
    (rect.right - rect.left).max(util::scale(300))
}

fn layout_snippets(state: &mut SettingsState, height: i32) {
    let c = &state.snippets;
    let width = page_width(state, PAGE_SNIPPETS);
    let m = util::scale(18);
    let gap = util::scale(18);
    let left = util::scale(236).min((width - m * 2 - gap) * 40 / 100);
    let right_x = m + left + gap;
    let right_width = (width - right_x - m).max(util::scale(220));

    move_window(c.search, m, util::scale(18), left, util::scale(30));
    move_window(c.count, m, util::scale(54), left, util::scale(20));
    let list_y = util::scale(78);
    let list_height = (height - list_y - util::scale(54)).max(util::scale(100));
    move_window(c.list, m, list_y, left, list_height);
    let small_button = (left - util::scale(8)) / 2;
    move_window(
        c.new,
        m,
        height - util::scale(42),
        small_button,
        util::scale(28),
    );
    move_window(
        c.duplicate,
        m + small_button + util::scale(8),
        height - util::scale(42),
        small_button,
        util::scale(28),
    );

    move_window(
        c.editor_heading,
        right_x,
        util::scale(16),
        right_width,
        util::scale(24),
    );
    let key_width = util::scale(154).min(right_width * 40 / 100);
    let name_width = right_width - key_width - util::scale(12);
    move_window(
        c.name_label,
        right_x,
        util::scale(50),
        name_width,
        util::scale(18),
    );
    move_window(
        c.name,
        right_x,
        util::scale(70),
        name_width,
        util::scale(30),
    );
    move_window(
        c.key_label,
        right_x + name_width + util::scale(12),
        util::scale(50),
        key_width,
        util::scale(18),
    );
    move_window(
        c.key,
        right_x + name_width + util::scale(12),
        util::scale(70),
        key_width,
        util::scale(200),
    );
    move_window(
        c.text_label,
        right_x,
        util::scale(112),
        right_width,
        util::scale(20),
    );
    let text_y = util::scale(136);
    let text_height = (height - text_y - util::scale(158)).max(util::scale(24));
    move_window(c.text, right_x, text_y, right_width, text_height);
    let checks_y = text_y + text_height + util::scale(10);
    move_window(
        c.send_enter,
        right_x,
        checks_y,
        right_width / 2,
        util::scale(22),
    );
    move_window(
        c.enabled,
        right_x + right_width / 2,
        checks_y,
        right_width / 2,
        util::scale(22),
    );
    let variable_label_y = height - util::scale(102);
    move_window(
        c.variable_label,
        right_x,
        variable_label_y,
        right_width,
        util::scale(18),
    );
    let insert_width = util::scale(92);
    move_window(
        c.variable,
        right_x,
        height - util::scale(80),
        right_width - insert_width - util::scale(8),
        util::scale(200),
    );
    move_window(
        c.insert_variable,
        right_x + right_width - insert_width,
        height - util::scale(80),
        insert_width,
        util::scale(28),
    );
    move_window(
        c.editor_hint,
        right_x,
        height - util::scale(38),
        right_width - util::scale(104),
        util::scale(22),
    );
    move_window(
        c.delete,
        right_x + right_width - util::scale(96),
        height - util::scale(42),
        util::scale(96),
        util::scale(28),
    );
}

fn layout_behavior(state: &mut SettingsState, height: i32) {
    let c = &state.behavior;
    let width = page_width(state, PAGE_BEHAVIOR);
    if height < util::scale(440) {
        layout_behavior_compact(c, width, height);
        return;
    }
    let m = util::scale(22);
    let gap = util::scale(28);
    let column = (width - m * 2 - gap) / 2;
    let right = m + column + gap;
    move_window(c.input_heading, m, util::scale(18), column, util::scale(24));
    move_window(
        c.input_method_label,
        m,
        util::scale(54),
        column,
        util::scale(20),
    );
    move_window(c.input_method, m, util::scale(76), column, util::scale(200));
    move_window(
        c.restore_clipboard,
        m,
        util::scale(120),
        column,
        util::scale(24),
    );
    move_window(
        c.activation_heading,
        m,
        util::scale(164),
        column,
        util::scale(24),
    );
    move_window(
        c.activation_label,
        m,
        util::scale(200),
        column,
        util::scale(20),
    );
    move_window(c.activation, m, util::scale(222), column, util::scale(200));
    move_window(c.hold_label, m, util::scale(266), column, util::scale(20));
    move_window(
        c.hold_ms,
        m,
        util::scale(288),
        util::scale(84),
        util::scale(30),
    );
    move_window(
        c.hold_hint,
        m + util::scale(94),
        util::scale(292),
        column - util::scale(94),
        util::scale(22),
    );
    move_window(c.autostart, m, util::scale(340), column, util::scale(42));

    move_window(
        c.hotkeys_heading,
        right,
        util::scale(18),
        column,
        util::scale(24),
    );
    move_window(
        c.list_hotkey_label,
        right,
        util::scale(54),
        column,
        util::scale(20),
    );
    move_window(
        c.list_hotkey,
        right,
        util::scale(76),
        column,
        util::scale(30),
    );
    move_window(
        c.config_hotkey_label,
        right,
        util::scale(118),
        column,
        util::scale(20),
    );
    move_window(
        c.config_hotkey,
        right,
        util::scale(140),
        column,
        util::scale(30),
    );
    move_window(
        c.history_hotkey_label,
        right,
        util::scale(182),
        column,
        util::scale(20),
    );
    move_window(
        c.history_hotkey,
        right,
        util::scale(204),
        column,
        util::scale(30),
    );
    move_window(
        c.ignore_label,
        right,
        util::scale(246),
        column,
        util::scale(20),
    );
    move_window(
        c.ignore_apps,
        right,
        util::scale(268),
        column,
        util::scale(88),
    );
    move_window(
        c.ignore_hint,
        right,
        util::scale(366),
        column,
        util::scale(64),
    );
}

/// Compact two-column layout used at the minimum window height. Keeping this
/// separate from the roomy default avoids overlapping the lower checkboxes and
/// preserves access to every field without shrinking fonts.
fn layout_behavior_compact(c: &BehaviorControls, width: i32, height: i32) {
    let m = util::scale(20);
    let gap = util::scale(22);
    let column = (width - m * 2 - gap) / 2;
    let right = m + column + gap;
    move_window(c.input_heading, m, util::scale(12), column, util::scale(22));
    move_window(
        c.input_method_label,
        m,
        util::scale(44),
        column,
        util::scale(18),
    );
    move_window(c.input_method, m, util::scale(64), column, util::scale(200));
    move_window(
        c.restore_clipboard,
        m,
        util::scale(104),
        column,
        util::scale(22),
    );
    move_window(
        c.activation_heading,
        m,
        util::scale(138),
        column,
        util::scale(22),
    );
    move_window(
        c.activation_label,
        m,
        util::scale(168),
        column,
        util::scale(18),
    );
    move_window(c.activation, m, util::scale(188), column, util::scale(200));
    move_window(c.hold_label, m, util::scale(216), column, util::scale(18));
    move_window(
        c.hold_ms,
        m,
        util::scale(236),
        util::scale(84),
        util::scale(28),
    );
    move_window(
        c.hold_hint,
        m + util::scale(92),
        util::scale(240),
        column - util::scale(92),
        util::scale(20),
    );
    move_window(
        c.autostart,
        m,
        height - util::scale(32),
        column,
        util::scale(24),
    );

    move_window(
        c.hotkeys_heading,
        right,
        util::scale(12),
        column,
        util::scale(22),
    );
    move_window(
        c.list_hotkey_label,
        right,
        util::scale(44),
        column,
        util::scale(18),
    );
    move_window(
        c.list_hotkey,
        right,
        util::scale(64),
        column,
        util::scale(28),
    );
    move_window(
        c.config_hotkey_label,
        right,
        util::scale(104),
        column,
        util::scale(18),
    );
    move_window(
        c.config_hotkey,
        right,
        util::scale(124),
        column,
        util::scale(28),
    );
    move_window(
        c.history_hotkey_label,
        right,
        util::scale(164),
        column,
        util::scale(18),
    );
    move_window(
        c.history_hotkey,
        right,
        util::scale(184),
        column,
        util::scale(28),
    );
    move_window(
        c.ignore_label,
        right,
        util::scale(224),
        column,
        util::scale(18),
    );
    move_window(
        c.ignore_apps,
        right,
        util::scale(244),
        column,
        util::scale(56),
    );
    move_window(
        c.ignore_hint,
        right,
        util::scale(306),
        column,
        (height - util::scale(306 + 8)).max(util::scale(18)),
    );
}

fn layout_sync(state: &mut SettingsState, height: i32) {
    let c = &state.sync;
    let width = page_width(state, PAGE_SYNC);
    if height < util::scale(480) {
        layout_sync_compact(c, width);
        return;
    }
    let m = util::scale(22);
    let inner = width - m * 2;
    let gap = util::scale(18);
    let column = (inner - gap) / 2;
    let right = m + column + gap;
    // SAFETY: the hint is a live child; restore it after a compact layout.
    unsafe {
        ShowWindow(c.certificate_fingerprint, SW_SHOW);
    }
    let test_width = util::scale(108);
    move_window(
        c.enabled,
        m,
        util::scale(16),
        inner - test_width - util::scale(12),
        util::scale(26),
    );
    move_window(
        c.test,
        width - m - test_width,
        util::scale(16),
        test_width,
        util::scale(30),
    );
    move_window(c.url_label, m, util::scale(54), inner, util::scale(20));
    move_window(c.url, m, util::scale(76), inner, util::scale(30));
    move_window(c.user_label, m, util::scale(118), column, util::scale(20));
    move_window(c.user, m, util::scale(140), column, util::scale(30));
    move_window(
        c.password_label,
        right,
        util::scale(118),
        column,
        util::scale(20),
    );
    move_window(c.password, right, util::scale(140), column, util::scale(30));
    move_window(
        c.password_state,
        m,
        util::scale(182),
        inner - util::scale(142),
        util::scale(22),
    );
    move_window(
        c.clear_password,
        width - m - util::scale(132),
        util::scale(178),
        util::scale(132),
        util::scale(28),
    );
    move_window(
        c.interval_label,
        m,
        util::scale(218),
        column,
        util::scale(20),
    );
    move_window(c.interval, m, util::scale(240), column, util::scale(30));
    move_window(
        c.timeout_label,
        right,
        util::scale(218),
        column,
        util::scale(20),
    );
    move_window(c.timeout, right, util::scale(240), column, util::scale(30));
    move_window(c.encrypt, m, util::scale(278), inner, util::scale(24));
    move_window(
        c.encryption_password_label,
        m,
        util::scale(304),
        column,
        util::scale(20),
    );
    move_window(
        c.encryption_password_state,
        right,
        util::scale(304),
        column,
        util::scale(20),
    );
    move_window(
        c.encryption_password,
        m,
        util::scale(326),
        column,
        util::scale(30),
    );
    move_window(
        c.clear_encryption_password,
        right,
        util::scale(326),
        column,
        util::scale(30),
    );
    move_window(
        c.sync_on_change,
        m,
        util::scale(368),
        inner,
        util::scale(24),
    );
    move_window(c.backup, m, util::scale(396), inner, util::scale(24));
    move_window(
        c.certificate_fingerprint,
        m,
        height - util::scale(30),
        inner,
        util::scale(30),
    );
}

/// Compact sync form: the test action moves beside the enable checkbox so it
/// cannot collide with the two lower option checkboxes.
fn layout_sync_compact(c: &SyncControls, width: i32) {
    let m = util::scale(20);
    let inner = width - m * 2;
    let gap = util::scale(16);
    let column = (inner - gap) / 2;
    let right = m + column + gap;
    let test_width = util::scale(108);
    move_window(
        c.enabled,
        m,
        util::scale(12),
        inner - test_width - util::scale(12),
        util::scale(26),
    );
    move_window(
        c.test,
        width - m - test_width,
        util::scale(12),
        test_width,
        util::scale(28),
    );
    move_window(c.url_label, m, util::scale(44), inner, util::scale(18));
    move_window(c.url, m, util::scale(64), inner, util::scale(28));
    move_window(c.user_label, m, util::scale(96), column, util::scale(18));
    move_window(c.user, m, util::scale(116), column, util::scale(28));
    move_window(
        c.password_label,
        right,
        util::scale(96),
        column,
        util::scale(18),
    );
    move_window(c.password, right, util::scale(116), column, util::scale(28));
    move_window(
        c.password_state,
        m,
        util::scale(144),
        inner - util::scale(142),
        util::scale(20),
    );
    move_window(
        c.clear_password,
        width - m - util::scale(132),
        util::scale(144),
        util::scale(132),
        util::scale(28),
    );
    move_window(
        c.interval_label,
        m,
        util::scale(164),
        column,
        util::scale(18),
    );
    move_window(c.interval, m, util::scale(184), column, util::scale(28));
    move_window(
        c.timeout_label,
        right,
        util::scale(164),
        column,
        util::scale(18),
    );
    move_window(c.timeout, right, util::scale(184), column, util::scale(28));
    move_window(c.encrypt, m, util::scale(216), inner, util::scale(22));
    move_window(
        c.encryption_password_label,
        m,
        util::scale(238),
        column,
        util::scale(18),
    );
    move_window(
        c.encryption_password_state,
        right,
        util::scale(238),
        column,
        util::scale(18),
    );
    move_window(
        c.encryption_password,
        m,
        util::scale(258),
        column,
        util::scale(28),
    );
    move_window(
        c.clear_encryption_password,
        right,
        util::scale(258),
        column,
        util::scale(28),
    );
    move_window(
        c.sync_on_change,
        m,
        util::scale(266),
        inner,
        util::scale(20),
    );
    move_window(c.backup, m, util::scale(288), inner, util::scale(20));
    move_window(
        c.certificate_fingerprint,
        m,
        util::scale(312),
        inner,
        util::scale(26),
    );
}

fn select_page(state: &mut SettingsState, page: usize) {
    if page >= state.pages.len() {
        return;
    }
    state.current_page = page;
    let (title, subtitle) = match page {
        PAGE_SNIPPETS => ("片段库", "集中管理常用文字；选中即可编辑，应用后立即生效。"),
        PAGE_BEHAVIOR => (
            "输入与快捷键",
            "调整输入体验、触发节奏和全局快捷键，让 Vibekey 更贴合你的习惯。",
        ),
        _ => ("同步与备份", "两类密码分开保存；远端配置正文可加密保护。"),
    };
    set_text(state.header_title, title);
    set_text(state.header_subtitle, subtitle);
    for (index, hwnd) in state.pages.iter().enumerate() {
        // SAFETY: page handles are live children.
        unsafe {
            ShowWindow(*hwnd, if index == page { SW_SHOW } else { SW_HIDE });
            InvalidateRect(state.nav[index], std::ptr::null(), 1);
        }
    }
}

fn mark_dirty(state: &mut SettingsState) {
    if state.loading {
        return;
    }
    state.dirty = true;
    set_text(state.status, "有未保存的更改 · 应用后立即生效");
}

fn refresh_snippet_list(state: &mut SettingsState, preferred: Option<usize>) {
    let previous_loading = state.loading;
    state.loading = true;
    let filter = state.snippet_filter.trim().to_lowercase();
    state.snippet_indices = state
        .config
        .snippet
        .iter()
        .enumerate()
        .filter(|(_, snippet)| {
            if filter.is_empty() {
                return true;
            }
            let key = format!("alt+{}", snippet.key);
            snippet.name.to_lowercase().contains(&filter)
                || snippet.text.to_lowercase().contains(&filter)
                || key.contains(&filter)
        })
        .map(|(index, _)| index)
        .collect();

    // SAFETY: the list box is live and copies every string before returning.
    unsafe {
        SendMessageW(state.snippets.list, LB_RESETCONTENT, 0, 0);
        for &index in &state.snippet_indices {
            let snippet = &state.config.snippet[index];
            let name = if snippet.name.trim().is_empty() {
                "未命名片段"
            } else {
                snippet.name.trim()
            };
            let text = wide(name);
            SendMessageW(state.snippets.list, LB_ADDSTRING, 0, text.as_ptr() as isize);
        }
    }

    let wanted = preferred.or(state.selected_snippet);
    let selected = wanted
        .filter(|index| state.snippet_indices.contains(index))
        .or_else(|| state.snippet_indices.first().copied());
    state.selected_snippet = selected;
    let list_index = selected.and_then(|config_index| {
        state
            .snippet_indices
            .iter()
            .position(|&index| index == config_index)
    });
    // SAFETY: the list box is live. LB_SETCURSEL does not emit another command.
    unsafe {
        SendMessageW(
            state.snippets.list,
            LB_SETCURSEL,
            list_index.map(|value| value as isize).unwrap_or(-1) as usize,
            0,
        );
    }
    let count = if filter.is_empty() {
        format!("{} 个片段 · 可搜索", state.config.snippet.len())
    } else {
        format!(
            "显示 {} / {} 个片段",
            state.snippet_indices.len(),
            state.config.snippet.len()
        )
    };
    set_text(state.snippets.count, &count);
    load_snippet_editor(state);
    state.loading = previous_loading;
}

fn load_snippet_editor(state: &mut SettingsState) {
    let previous_loading = state.loading;
    state.loading = true;
    let selected = state
        .selected_snippet
        .and_then(|index| state.config.snippet.get(index));
    let enabled = selected.is_some();
    let (name, key, text, send_enter, snippet_enabled) = match selected {
        Some(snippet) => (
            snippet.name.as_str(),
            snippet.key.as_str(),
            snippet.text.as_str(),
            snippet.send_enter,
            snippet.enabled,
        ),
        None => ("", "", "", false, true),
    };
    set_text(state.snippets.name, name);
    set_text(state.snippets.text, text);
    let key_index = SLOT_KEYS
        .iter()
        .position(|candidate| *candidate == key)
        .map(|index| index + 1)
        .unwrap_or(0);
    // SAFETY: all controls are live children; these setters do not pump messages.
    unsafe {
        SendMessageW(state.snippets.key, CB_SETCURSEL, key_index, 0);
        SendMessageW(
            state.snippets.send_enter,
            BM_SETCHECK,
            if send_enter { 1 } else { 0 },
            0,
        );
        SendMessageW(
            state.snippets.enabled,
            BM_SETCHECK,
            if snippet_enabled { 1 } else { 0 },
            0,
        );
    }
    for control in [
        state.snippets.name,
        state.snippets.key,
        state.snippets.text,
        state.snippets.send_enter,
        state.snippets.enabled,
        state.snippets.variable,
        state.snippets.insert_variable,
        state.snippets.delete,
        state.snippets.duplicate,
    ] {
        set_enabled(control, enabled);
    }
    state.loading = previous_loading;
}

fn same_snippet(left: &Snippet, right: &Snippet) -> bool {
    left.key == right.key
        && left.name == right.name
        && left.text == right.text
        && left.send_enter == right.send_enter
        && left.enabled == right.enabled
}

fn update_selected_snippet(state: &mut SettingsState, id: i32) {
    let Some(index) = state.selected_snippet else {
        return;
    };
    let Some(snippet) = state.config.snippet.get_mut(index) else {
        return;
    };
    match id {
        ID_SNIPPET_NAME => snippet.name = window_text(state.snippets.name),
        ID_SNIPPET_TEXT => snippet.text = window_text(state.snippets.text),
        ID_SNIPPET_KEY => {
            let selected = combo_selection(state.snippets.key);
            snippet.key = if selected == 0 {
                String::new()
            } else {
                SLOT_KEYS.get(selected - 1).unwrap_or(&"").to_string()
            };
        }
        ID_SNIPPET_SEND_ENTER => snippet.send_enter = checked(state.snippets.send_enter),
        ID_SNIPPET_ENABLED => snippet.enabled = checked(state.snippets.enabled),
        _ => return,
    }
    mark_dirty(state);
    // Owner-drawn rows read directly from Config, so only a redraw is needed;
    // rebuilding the list on each keystroke would disturb selection/focus.
    // SAFETY: the list handle is live.
    unsafe {
        InvalidateRect(state.snippets.list, std::ptr::null(), 1);
    }
}

fn new_snippet(state: &mut SettingsState) {
    let index = state.config.snippet.len();
    state.config.snippet.push(Snippet {
        name: "新片段".to_string(),
        ..Snippet::default()
    });
    state.snippet_filter.clear();
    set_text(state.snippets.search, "");
    refresh_snippet_list(state, Some(index));
    mark_dirty(state);
    // SAFETY: the edit control is live; select its default name for fast rename.
    unsafe {
        SetFocus(state.snippets.name);
        SendMessageW(state.snippets.name, EM_SETSEL, 0, -1);
    }
}

fn duplicate_snippet(state: &mut SettingsState) {
    let Some(index) = state.selected_snippet else {
        return;
    };
    let Some(mut snippet) = state.config.snippet.get(index).cloned() else {
        return;
    };
    snippet.key.clear();
    snippet.name = if snippet.name.trim().is_empty() {
        "未命名片段 副本".to_string()
    } else {
        format!("{} 副本", snippet.name.trim())
    };
    state.config.snippet.insert(index + 1, snippet);
    state.snippet_filter.clear();
    set_text(state.snippets.search, "");
    refresh_snippet_list(state, Some(index + 1));
    mark_dirty(state);
}

fn delete_snippet(state: &mut SettingsState) {
    let Some(index) = state.selected_snippet else {
        return;
    };
    let name = state
        .config
        .snippet
        .get(index)
        .map(|snippet| snippet.name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or("未命名片段");
    let text = wide(&format!(
        "确定删除“{name}”吗？\n\n点击“应用”后才会写入配置文件。"
    ));
    let caption = wide("删除片段");
    // SAFETY: both strings are valid and the owner is a live settings window.
    let answer = unsafe {
        MessageBoxW(
            state.hwnd,
            text.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    };
    if answer != IDYES {
        return;
    }
    state.config.snippet.remove(index);
    state.selected_snippet = None;
    refresh_snippet_list(
        state,
        Some(index.min(state.config.snippet.len().saturating_sub(1))),
    );
    mark_dirty(state);
}

fn insert_variable(state: &mut SettingsState) {
    if state.selected_snippet.is_none() {
        return;
    }
    let index = combo_selection(state.snippets.variable).min(VARIABLES.len() - 1);
    let token = wide(VARIABLES[index].0);
    // SAFETY: EM_REPLACESEL inserts at the current caret and copies the string.
    unsafe {
        SendMessageW(
            state.snippets.text,
            EM_REPLACESEL,
            1,
            token.as_ptr() as isize,
        );
        SetFocus(state.snippets.text);
    }
    update_selected_snippet(state, ID_SNIPPET_TEXT);
}

fn update_password_state(state: &mut SettingsState) {
    let message = if state.clear_password_requested {
        "保存后清除登录密码".to_string()
    } else if !window_text(state.sync.password).is_empty() {
        "将使用当前输入的 WebDAV 密码".to_string()
    } else {
        match state.secrets.load(SecretKind::WebDav) {
            Ok(Some(_)) => "登录密码已安全保存；留空不修改".to_string(),
            Ok(None) => "匿名连接可留空".to_string(),
            Err(_) => "无法读取登录密码状态".to_string(),
        }
    };
    set_text(state.sync.password_state, &message);

    let message = if state.clear_encryption_password_requested {
        "保存后清除加密密码".to_string()
    } else if !window_text(state.sync.encryption_password).is_empty() {
        "新密码将在成功同步后完成替换".to_string()
    } else {
        match state.secrets.load_encryption_passwords() {
            Ok((Some(_), Some(_))) => "等待成功同步后删除旧密码".to_string(),
            Ok((Some(_), None)) => "加密密码已安全保存；留空不修改".to_string(),
            Ok((None, _)) => "启用加密前必须设置独立密码".to_string(),
            Err(_) => "无法读取加密密码状态".to_string(),
        }
    };
    set_text(state.sync.encryption_password_state, &message);
}

fn confirm_discard(state: &SettingsState) -> bool {
    if !state.dirty {
        return true;
    }
    let text = wide("还有未应用的更改。确定放弃这些更改并关闭吗？");
    let caption = wide("未保存的更改");
    // SAFETY: both strings are valid and the owner window is live.
    unsafe {
        MessageBoxW(
            state.hwnd,
            text.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDYES
    }
}

unsafe extern "system" fn settings_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = util::window_userdata(hwnd) as *mut SettingsState;
    match message {
        WM_COMMAND => {
            if pointer.is_null() {
                return 0;
            }
            let state = &mut *pointer;
            let id = (wparam & 0xFFFF) as i32;
            let notification = ((wparam as usize >> 16) & 0xFFFF) as u32;
            match id {
                ID_SAVE => {
                    if save(state) {
                        DestroyWindow(hwnd);
                    }
                }
                ID_APPLY => {
                    save(state);
                }
                ID_CANCEL => {
                    if confirm_discard(state) {
                        DestroyWindow(hwnd);
                    }
                }
                ID_NAV_SNIPPETS | ID_NAV_BEHAVIOR | ID_NAV_SYNC => {
                    select_page(state, (id - ID_NAV_SNIPPETS) as usize);
                }
                ID_SNIPPET_NEW => new_snippet(state),
                ID_SNIPPET_DUPLICATE => duplicate_snippet(state),
                ID_SNIPPET_DELETE => delete_snippet(state),
                ID_SNIPPET_INSERT_VARIABLE => insert_variable(state),
                ID_SNIPPET_LIST if notification == LBN_SELCHANGE as u32 => {
                    let selected = SendMessageW(state.snippets.list, LB_GETCURSEL, 0, 0);
                    state.selected_snippet = if selected < 0 {
                        None
                    } else {
                        state.snippet_indices.get(selected as usize).copied()
                    };
                    load_snippet_editor(state);
                }
                ID_SNIPPET_SEARCH if notification == EN_CHANGE => {
                    state.snippet_filter = window_text(state.snippets.search);
                    refresh_snippet_list(state, None);
                }
                ID_SNIPPET_NAME | ID_SNIPPET_TEXT if notification == EN_CHANGE => {
                    update_selected_snippet(state, id);
                }
                ID_SNIPPET_KEY if notification == CBN_SELCHANGE as u32 => {
                    update_selected_snippet(state, id);
                }
                ID_SNIPPET_SEND_ENTER | ID_SNIPPET_ENABLED => {
                    update_selected_snippet(state, id);
                }
                ID_CLEAR_PASSWORD => {
                    state.clear_password_requested = true;
                    set_text(state.sync.password, "");
                    update_password_state(state);
                    mark_dirty(state);
                }
                ID_PASSWORD if notification == EN_CHANGE => {
                    if !window_text(state.sync.password).is_empty() {
                        state.clear_password_requested = false;
                    }
                    update_password_state(state);
                    mark_dirty(state);
                }
                ID_CLEAR_ENCRYPTION_PASSWORD => {
                    state.clear_encryption_password_requested = true;
                    set_text(state.sync.encryption_password, "");
                    update_password_state(state);
                    mark_dirty(state);
                }
                ID_ENCRYPTION_PASSWORD if notification == EN_CHANGE => {
                    if !window_text(state.sync.encryption_password).is_empty() {
                        state.clear_encryption_password_requested = false;
                    }
                    update_password_state(state);
                    mark_dirty(state);
                }
                ID_TEST => test_connection(state),
                ID_OPEN_CONFIG => {
                    if let Err(error) = std::process::Command::new("notepad.exe")
                        .arg(&state.config_path)
                        .spawn()
                    {
                        util::message_box_error(
                            hwnd,
                            &format!("无法打开配置文件：{error}"),
                            "打开 TOML",
                        );
                    }
                }
                ID_SYNC_ENABLED | ID_SYNC_ON_CHANGE | ID_BACKUP | ID_ENCRYPT
                | ID_RESTORE_CLIPBOARD | ID_AUTOSTART => mark_dirty(state),
                ID_URL
                | ID_USER
                | ID_INTERVAL
                | ID_TIMEOUT
                | ID_HOLD_MS
                | ID_LIST_HOTKEY
                | ID_CONFIG_HOTKEY
                | ID_HISTORY_HOTKEY
                | ID_IGNORE_APPS
                | ID_CERTIFICATE_SHA256
                    if notification == EN_CHANGE =>
                {
                    mark_dirty(state)
                }
                ID_INPUT_METHOD | ID_ACTIVATION if notification == CBN_SELCHANGE as u32 => {
                    mark_dirty(state)
                }
                _ => {}
            }
            0
        }
        WM_APP_SHOW_SNIPPET => {
            if !pointer.is_null() {
                let state = &mut *pointer;
                select_page(state, PAGE_SNIPPETS);
                if wparam == 0 {
                    new_snippet(state);
                } else {
                    let index = wparam - 1;
                    let mut selected = None;
                    if lparam != 0 {
                        // SAFETY: show_snippet_editor passes a live &Snippet
                        // through synchronous SendMessageW; it remains valid
                        // until this handler returns.
                        let expected = &*(lparam as *const Snippet);
                        selected = state
                            .config
                            .snippet
                            .get(index)
                            .filter(|snippet| same_snippet(snippet, expected))
                            .map(|_| index);
                        if selected.is_none() {
                            let mut matches = state
                                .config
                                .snippet
                                .iter()
                                .enumerate()
                                .filter(|(_, snippet)| same_snippet(snippet, expected));
                            if let Some((matching_index, _)) = matches.next() {
                                if matches.next().is_none() {
                                    selected = Some(matching_index);
                                }
                            }
                        }
                    }
                    if let Some(index) = selected {
                        state.snippet_filter.clear();
                        set_text(state.snippets.search, "");
                        refresh_snippet_list(state, Some(index));
                    } else {
                        // Never guess from a stale position: unsaved structural
                        // edits or an external reload may have shifted indices.
                        set_text(
                            state.status,
                            "无法安全定位弹窗中的片段，请在左侧列表手动选择",
                        );
                        log::warn!("quick edit could not resolve snippet identity");
                    }
                }
                util::force_foreground(hwnd);
            }
            0
        }
        WM_APP_TEST_RESULT => {
            if !pointer.is_null() {
                let state = &mut *pointer;
                state.test_running = false;
                set_enabled(state.sync.test, true);
                let result = state
                    .test_result
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take());
                if let Some((ok, message)) = result {
                    if ok {
                        util::message_box_info(hwnd, &message, "测试连接");
                    } else {
                        util::message_box_error(hwnd, &message, "测试连接");
                    }
                }
            }
            0
        }
        WM_SIZE => {
            if !pointer.is_null() {
                layout(&mut *pointer);
            }
            0
        }
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            if !info.is_null() {
                let work = util::monitor_work_area(util::mouse_pos());
                let max_width = (work.right - work.left - util::scale(16)).max(util::scale(480));
                let max_height = (work.bottom - work.top - util::scale(8)).max(util::scale(320));
                (*info).ptMinTrackSize.x = util::scale(760).min(max_width);
                (*info).ptMinTrackSize.y = util::scale(540).min(max_height);
            }
            0
        }
        WM_MEASUREITEM => {
            if wparam == ID_SNIPPET_LIST as usize {
                let item = lparam as *mut MEASUREITEMSTRUCT;
                if !item.is_null() {
                    (*item).itemHeight = util::scale(54) as u32;
                }
                return 1;
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_DRAWITEM => {
            if pointer.is_null() {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            let item = lparam as *const DRAWITEMSTRUCT;
            if item.is_null() {
                return 0;
            }
            match (*item).CtlID as i32 {
                ID_NAV_SNIPPETS | ID_NAV_BEHAVIOR | ID_NAV_SYNC => {
                    draw_nav_button(&*pointer, &*item);
                    1
                }
                ID_SAVE => {
                    draw_primary_button(&*item);
                    1
                }
                ID_SNIPPET_LIST => {
                    draw_snippet_item(&*pointer, &*item);
                    1
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
        WM_PAINT => {
            paint_chrome(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_CTLCOLORSTATIC => control_color(message, wparam, lparam),
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
            control_color(message, wparam, lparam)
        }
        WM_CLOSE => {
            if pointer.is_null() || confirm_discard(&*pointer) {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: window procedures only run on the main thread; the app may
            // already be gone during shutdown.
            if let Some(app) = crate::app::try_app_mut() {
                app.settings = std::ptr::null_mut();
            }
            0
        }
        WM_NCDESTROY => {
            if !pointer.is_null() {
                util::set_window_userdata(hwnd, 0);
                // SAFETY: created by Box::into_raw in open and released once.
                drop(Box::from_raw(pointer));
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe extern "system" fn page_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Control notifications and owner-draw requests go to the immediate
        // parent page. Forward them to the top-level settings window, which
        // owns the shared SettingsState and implements the actual commands.
        WM_COMMAND | WM_DRAWITEM | WM_MEASUREITEM => {
            SendMessageW(GetParent(hwnd), message, wparam, lparam)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
            control_color(message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn control_color(message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let control = lparam as HWND;
    // SAFETY: the DC belongs to the control being coloured and remains owned by
    // Windows.  Static brushes are shared for the process lifetime.
    unsafe {
        SetBkMode(wparam as _, TRANSPARENT as i32);
        match message {
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let id = GetDlgCtrlID(control);
                let color: COLORREF = if matches!(
                    id,
                    ID_HEADER_SUBTITLE
                        | ID_STATUS
                        | ID_PASSWORD_STATE
                        | ID_ENCRYPTION_PASSWORD_STATE
                ) {
                    theme::TEXT_MUTED
                } else {
                    theme::TEXT
                };
                SetTextColor(wparam as _, color);
                let parent = GetParent(control);
                let parent_class = util::window_class_name(parent);
                if parent_class == CLASS_NAME {
                    SetBkColor(wparam as _, theme::BACKGROUND);
                    theme::background_brush() as LRESULT
                } else {
                    SetBkColor(wparam as _, theme::SURFACE);
                    theme::surface_brush() as LRESULT
                }
            }
            _ => {
                SetTextColor(wparam as _, theme::TEXT);
                SetBkColor(wparam as _, theme::SURFACE);
                theme::surface_brush() as LRESULT
            }
        }
    }
}

fn paint_chrome(hwnd: HWND) {
    // SAFETY: BeginPaint/EndPaint are paired and all drawing stays inside the
    // returned paint DC.  Child controls paint themselves afterwards.
    unsafe {
        let mut paint: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut paint);
        if hdc.is_null() {
            EndPaint(hwnd, &paint);
            return;
        }
        let mut client = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        GetClientRect(hwnd, &mut client);
        theme::fill(hdc, &client, theme::BACKGROUND);
        let sidebar = RECT {
            left: 0,
            top: 0,
            right: util::scale(SIDEBAR_WIDTH),
            bottom: client.bottom,
        };
        theme::fill(hdc, &sidebar, theme::SIDEBAR);
        let logo = util::rect(
            util::scale(22),
            util::scale(24),
            util::scale(66),
            util::scale(68),
        );
        theme::fill_rounded(hdc, &logo, util::scale(12), theme::ACCENT);
        theme::text_centered(hdc, &logo, "V", theme::title_font(), theme::SURFACE);
        let brand = util::rect(
            util::scale(78),
            util::scale(24),
            sidebar.right - util::scale(12),
            util::scale(48),
        );
        theme::text(
            hdc,
            &brand,
            "Vibekey",
            theme::semibold_font(),
            theme::TEXT_ON_DARK,
            true,
        );
        let tagline = util::rect(
            util::scale(78),
            util::scale(46),
            sidebar.right - util::scale(8),
            util::scale(70),
        );
        theme::text(
            hdc,
            &tagline,
            "输入，快人一步",
            theme::small_font(),
            theme::rgb(157, 170, 198),
            true,
        );
        let footer = util::rect(
            util::scale(22),
            client.bottom - util::scale(74),
            sidebar.right - util::scale(16),
            client.bottom - util::scale(20),
        );
        theme::text_multiline(
            hdc,
            &footer,
            &format!(
                "Alt + `  打开片段\n双击托盘  管理设置\nv{}",
                env!("CARGO_PKG_VERSION")
            ),
            theme::small_font(),
            theme::rgb(157, 170, 198),
        );
        EndPaint(hwnd, &paint);
    }
}

unsafe fn draw_nav_button(state: &SettingsState, item: &DRAWITEMSTRUCT) {
    let selected_page = (item.CtlID as i32 - ID_NAV_SNIPPETS) as usize;
    let active = selected_page == state.current_page;
    let pressed = item.itemState & ODS_SELECTED != 0;
    let background = if active {
        theme::ACCENT
    } else if pressed {
        theme::SIDEBAR_HOVER
    } else {
        theme::SIDEBAR
    };
    theme::fill_rounded(item.hDC, &item.rcItem, util::scale(10), background);
    let mut text_rect = item.rcItem;
    text_rect.left += util::scale(16);
    text_rect.right -= util::scale(12);
    let text = window_text(item.hwndItem);
    theme::text(
        item.hDC,
        &text_rect,
        &text,
        theme::semibold_font(),
        if active {
            theme::SURFACE
        } else {
            theme::rgb(196, 205, 227)
        },
        true,
    );
    if active {
        let marker = util::rect(
            item.rcItem.left,
            item.rcItem.top + util::scale(10),
            item.rcItem.left + util::scale(3),
            item.rcItem.bottom - util::scale(10),
        );
        theme::fill(item.hDC, &marker, theme::SURFACE);
    }
}

unsafe fn draw_primary_button(item: &DRAWITEMSTRUCT) {
    let pressed = item.itemState & ODS_SELECTED != 0;
    theme::fill_rounded(
        item.hDC,
        &item.rcItem,
        util::scale(7),
        if pressed {
            theme::ACCENT_DARK
        } else {
            theme::ACCENT
        },
    );
    let text = window_text(item.hwndItem);
    theme::text_centered(
        item.hDC,
        &item.rcItem,
        &text,
        theme::semibold_font(),
        theme::SURFACE,
    );
    if item.itemState & windows_sys::Win32::UI::Controls::ODS_FOCUS != 0 {
        let mut focus = item.rcItem;
        focus.left += util::scale(4);
        focus.top += util::scale(4);
        focus.right -= util::scale(4);
        focus.bottom -= util::scale(4);
        DrawFocusRect(item.hDC, &focus);
    }
}

unsafe fn draw_snippet_item(state: &SettingsState, item: &DRAWITEMSTRUCT) {
    let selected = item.itemState & ODS_SELECTED != 0;
    theme::fill(
        item.hDC,
        &item.rcItem,
        if selected {
            theme::ACCENT_SOFT
        } else {
            theme::SURFACE
        },
    );
    let Some(&config_index) = state.snippet_indices.get(item.itemID as usize) else {
        return;
    };
    let Some(snippet) = state.config.snippet.get(config_index) else {
        return;
    };
    if selected {
        let marker = util::rect(
            item.rcItem.left,
            item.rcItem.top,
            item.rcItem.left + util::scale(3),
            item.rcItem.bottom,
        );
        theme::fill(item.hDC, &marker, theme::ACCENT);
    }
    let chip = util::rect(
        item.rcItem.left + util::scale(10),
        item.rcItem.top + util::scale(12),
        item.rcItem.left + util::scale(70),
        item.rcItem.bottom - util::scale(12),
    );
    theme::fill_rounded(
        item.hDC,
        &chip,
        util::scale(7),
        if snippet.enabled {
            theme::ACCENT_SOFT
        } else {
            theme::BACKGROUND
        },
    );
    let key = if snippet.key.is_empty() {
        "列表".to_string()
    } else {
        format!("Alt+{}", snippet.key)
    };
    theme::text_centered(
        item.hDC,
        &chip,
        &key,
        theme::small_font(),
        if snippet.enabled {
            theme::ACCENT_DARK
        } else {
            theme::TEXT_MUTED
        },
    );
    let name = if snippet.name.trim().is_empty() {
        "未命名片段"
    } else {
        snippet.name.trim()
    };
    let mut name_rect = item.rcItem;
    name_rect.left += util::scale(80);
    name_rect.top += util::scale(5);
    name_rect.bottom = name_rect.top + util::scale(24);
    name_rect.right -= util::scale(10);
    theme::text(
        item.hDC,
        &name_rect,
        name,
        theme::semibold_font(),
        if snippet.enabled {
            theme::TEXT
        } else {
            theme::TEXT_MUTED
        },
        true,
    );
    let first_line = snippet.text.lines().next().unwrap_or("").replace('\t', " ");
    let preview: String = first_line.chars().take(36).collect();
    let preview = if snippet.enabled {
        preview
    } else {
        format!("已停用 · {preview}")
    };
    let mut preview_rect = item.rcItem;
    preview_rect.left += util::scale(80);
    preview_rect.top += util::scale(29);
    preview_rect.right -= util::scale(10);
    preview_rect.bottom -= util::scale(3);
    theme::text(
        item.hDC,
        &preview_rect,
        &preview,
        theme::small_font(),
        theme::TEXT_MUTED,
        true,
    );
}

/// Validate and persist the working configuration.  Apply keeps the window
/// open; Save closes it after this function returns true.
fn save(state: &mut SettingsState) -> bool {
    let mut config = state.config.clone();
    config.sync.enabled = checked(state.sync.enabled);
    config.sync.url = window_text(state.sync.url).trim().to_string();
    config.sync.username = window_text(state.sync.user).trim().to_string();
    config.sync.sync_on_change = checked(state.sync.sync_on_change);
    config.sync.backup_on_conflict = checked(state.sync.backup);
    config.sync.encrypt = checked(state.sync.encrypt);
    config.sync.certificate_sha256 = window_text(state.sync.certificate_fingerprint)
        .trim()
        .to_string();
    config.start_with_windows = checked(state.behavior.autostart);
    config.restore_clipboard = checked(state.behavior.restore_clipboard);
    config.input_method = if combo_selection(state.behavior.input_method) == 0 {
        InputMethod::Paste
    } else {
        InputMethod::Type
    };
    config.activation = if combo_selection(state.behavior.activation) == 0 {
        ActivationMode::Immediate
    } else {
        ActivationMode::Hold
    };
    config.hotkeys.list = window_text(state.behavior.list_hotkey).trim().to_string();
    config.hotkeys.config = window_text(state.behavior.config_hotkey).trim().to_string();
    config.hotkeys.history = window_text(state.behavior.history_hotkey)
        .trim()
        .to_string();
    config.ignore_apps = window_text(state.behavior.ignore_apps)
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();

    let hold = window_text(state.behavior.hold_ms).trim().to_string();
    match hold.parse::<u32>() {
        Ok(value) => config.hold_ms = value,
        _ => {
            util::message_box_error(
                state.hwnd,
                "按住时长必须是非负整数（毫秒），建议使用 100–3000。",
                "保存失败",
            );
            select_page(state, PAGE_BEHAVIOR);
            return false;
        }
    }
    let interval = window_text(state.sync.interval).trim().to_string();
    match interval.parse::<u32>() {
        Ok(minutes) => config.sync.interval_minutes = minutes,
        Err(_) => {
            util::message_box_error(
                state.hwnd,
                "同步间隔必须是非负整数（分钟，0 表示不定时同步）。",
                "保存失败",
            );
            select_page(state, PAGE_SYNC);
            return false;
        }
    }
    let timeout = window_text(state.sync.timeout).trim().to_string();
    match timeout.parse::<u64>() {
        Ok(seconds) if seconds > 0 => config.sync.timeout_secs = seconds,
        _ => {
            util::message_box_error(
                state.hwnd,
                "连接超时必须是大于 0 的整数（秒）。",
                "保存失败",
            );
            select_page(state, PAGE_SYNC);
            return false;
        }
    }

    if !config.sync.certificate_sha256.is_empty()
        && CertificateFingerprint::parse(&config.sync.certificate_sha256).is_err()
    {
        util::message_box_error(
            state.hwnd,
            "证书指纹必须是 64 位十六进制字符，可包含冒号、空格或 SHA256: 前缀。",
            "保存失败",
        );
        select_page(state, PAGE_SYNC);
        return false;
    }

    let encryption_password = window_text(state.sync.encryption_password);
    if !encryption_password.is_empty() && validate_password(&encryption_password).is_err() {
        util::message_box_error(
            state.hwnd,
            "配置加密密码至少需要 8 个字符（中文也按字符计）。",
            "保存失败",
        );
        select_page(state, PAGE_SYNC);
        return false;
    }
    if !encryption_password.is_empty() {
        if let Ok((Some(current), Some(_))) = state.secrets.load_encryption_passwords() {
            if current != encryption_password {
                util::message_box_error(
                    state.hwnd,
                    "上一次加密密码轮换尚未确认。请先完成一次成功同步，再更换为新密码；这样可避免远端密码丢失。",
                    "保存失败",
                );
                select_page(state, PAGE_SYNC);
                return false;
            }
        }
    }

    if config.sync.enabled {
        if config.sync.url.is_empty() {
            util::message_box_error(state.hwnd, "启用同步时必须填写 WebDAV 地址。", "保存失败");
            select_page(state, PAGE_SYNC);
            return false;
        }
        if !(config.sync.url.starts_with("http://") || config.sync.url.starts_with("https://")) {
            util::message_box_error(
                state.hwnd,
                "WebDAV 地址必须以 http:// 或 https:// 开头。",
                "保存失败",
            );
            select_page(state, PAGE_SYNC);
            return false;
        }
        if config.sync.encrypt && encryption_password.is_empty() {
            if state.clear_encryption_password_requested {
                util::message_box_error(
                    state.hwnd,
                    "启用配置加密时不能清除加密密码。请先关闭加密或输入新密码。",
                    "保存失败",
                );
                select_page(state, PAGE_SYNC);
                return false;
            }
            match state.secrets.load(SecretKind::ConfigEncryption) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    util::message_box_error(
                        state.hwnd,
                        "启用配置加密前，请填写至少 8 个字符的独立同步加密密码。",
                        "保存失败",
                    );
                    select_page(state, PAGE_SYNC);
                    return false;
                }
                Err(error) => {
                    util::message_box_error(
                        state.hwnd,
                        &format!("无法读取已保存的加密密码：{error}"),
                        "保存失败",
                    );
                    return false;
                }
            }
        }
    }

    if let Err(errors) = Keymap::build(&config) {
        let details = errors
            .iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        util::message_box_error(
            state.hwnd,
            &format!("快捷键无效或与其它输入项重复，请先修正：\n\n{details}"),
            "保存失败",
        );
        return false;
    }

    if state.original_mtime != file_mtime(&state.config_path) {
        let text = wide(
            "配置文件已被外部程序或同步修改。继续应用会用当前窗口内容覆盖它。\n\n仍要继续吗？",
        );
        let caption = wide("配置已变化");
        // SAFETY: strings are valid and the owner is live.
        let answer = unsafe {
            MessageBoxW(
                state.hwnd,
                text.as_ptr(),
                caption.as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            )
        };
        if answer != IDYES {
            return false;
        }
    }

    let password = window_text(state.sync.password);
    if state.clear_password_requested {
        if let Err(error) = state.secrets.clear(SecretKind::WebDav) {
            util::message_box_error(
                state.hwnd,
                &format!("无法清除登录密码：{error}"),
                "保存失败",
            );
            return false;
        }
    } else if !password.is_empty() {
        if let Err(error) = state.secrets.store(SecretKind::WebDav, &password) {
            util::message_box_error(
                state.hwnd,
                &format!("无法保存登录密码：{error}"),
                "保存失败",
            );
            return false;
        }
    }
    if state.clear_encryption_password_requested {
        if let Err(error) = state.secrets.clear(SecretKind::ConfigEncryption) {
            util::message_box_error(
                state.hwnd,
                &format!("无法清除加密密码：{error}"),
                "保存失败",
            );
            return false;
        }
    } else if !encryption_password.is_empty() {
        if let Err(error) = state
            .secrets
            .store(SecretKind::ConfigEncryption, &encryption_password)
        {
            util::message_box_error(
                state.hwnd,
                &format!("无法保存加密密码：{error}"),
                "保存失败",
            );
            return false;
        }
    }

    if let Err(error) = config.save(&state.config_path) {
        util::message_box_error(
            state.hwnd,
            &format!("无法写入配置文件：{error}"),
            "保存失败",
        );
        return false;
    }

    if let Err(error) = autostart::set_enabled(config.start_with_windows) {
        util::message_box_warning(state.hwnd, &format!("开机启动设置失败：{error}"), "保存");
    }

    state.config = config.clone();
    state.original_mtime = file_mtime(&state.config_path);
    state.loading = true;
    set_text(state.sync.password, "");
    set_text(state.sync.encryption_password, "");
    state.clear_password_requested = false;
    state.clear_encryption_password_requested = false;
    update_password_state(state);
    state.loading = false;
    state.dirty = false;
    set_text(state.status, "已应用 · 片段和设置现在生效");

    // SAFETY: the main window handle is valid while the app is running.
    unsafe {
        PostMessageW(state.main_hwnd, crate::util::WM_APP_RELOAD, 0, 0);
        if config.sync.enabled && config.sync.sync_on_change {
            PostMessageW(state.main_hwnd, crate::util::WM_APP_SYNC_NOW, 0, 0);
        }
    }
    true
}

fn test_connection(state: &mut SettingsState) {
    if state.test_running {
        return;
    }
    let url = window_text(state.sync.url).trim().to_string();
    if url.is_empty() {
        util::message_box_warning(state.hwnd, "请先填写 WebDAV 地址。", "测试连接");
        return;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        util::message_box_warning(
            state.hwnd,
            "WebDAV 地址必须以 http:// 或 https:// 开头。",
            "测试连接",
        );
        return;
    }
    let username = window_text(state.sync.user).trim().to_string();
    let entered_password = window_text(state.sync.password);
    let password = if !entered_password.is_empty() {
        entered_password
    } else if state.clear_password_requested {
        util::message_box_warning(
            state.hwnd,
            "已选择清除密码，请先输入用于测试的密码。",
            "测试连接",
        );
        return;
    } else {
        match state.secrets.load(SecretKind::WebDav) {
            Ok(Some(saved)) => saved,
            Ok(None) if username.is_empty() => String::new(),
            Ok(None) => {
                util::message_box_warning(state.hwnd, "请先填写 WebDAV 密码。", "测试连接");
                return;
            }
            Err(error) => {
                util::message_box_error(
                    state.hwnd,
                    &format!("无法读取已保存的 WebDAV 密码：{error}"),
                    "测试连接",
                );
                return;
            }
        }
    };
    let encryption_enabled = checked(state.sync.encrypt);
    let entered_encryption_password = window_text(state.sync.encryption_password);
    if !entered_encryption_password.is_empty()
        && validate_password(&entered_encryption_password).is_err()
    {
        util::message_box_warning(
            state.hwnd,
            "配置加密密码至少需要 8 个字符（中文也按字符计）。",
            "测试连接",
        );
        return;
    }
    let (saved_current, saved_previous) = match state.secrets.load_encryption_passwords() {
        Ok(pair) => pair,
        Err(error) => {
            util::message_box_error(
                state.hwnd,
                &format!("无法读取已保存的加密密码：{error}"),
                "测试连接",
            );
            return;
        }
    };
    let encryption_passwords = if !entered_encryption_password.is_empty() {
        if saved_previous.is_some()
            && saved_current.as_deref() != Some(entered_encryption_password.as_str())
        {
            util::message_box_warning(
                state.hwnd,
                "上一次加密密码轮换尚未确认，请先成功同步，再测试另一个新密码。",
                "测试连接",
            );
            return;
        }
        let previous = if saved_current.as_deref() == Some(entered_encryption_password.as_str()) {
            saved_previous
        } else {
            // Testing a first replacement must model exactly what Apply will
            // persist: new current + the old current as one-time fallback.
            saved_current
        };
        [Some(entered_encryption_password), previous]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    } else if state.clear_encryption_password_requested {
        Vec::new()
    } else {
        [saved_current, saved_previous]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    };
    if encryption_enabled && encryption_passwords.is_empty() {
        util::message_box_warning(
            state.hwnd,
            "已启用配置加密，请先填写独立同步加密密码。",
            "测试连接",
        );
        return;
    }
    let timeout = window_text(state.sync.timeout)
        .trim()
        .parse::<u64>()
        .unwrap_or(20)
        .max(1);
    let certificate_sha256 = window_text(state.sync.certificate_fingerprint)
        .trim()
        .to_string();
    if !certificate_sha256.is_empty() && CertificateFingerprint::parse(&certificate_sha256).is_err()
    {
        util::message_box_warning(
            state.hwnd,
            "证书指纹必须是 64 位十六进制字符，可包含冒号、空格或 SHA256: 前缀。",
            "测试连接",
        );
        return;
    }

    let disable_encryption_allowed =
        vibekey_core::sync::last_state(&state.config_path).remote_encrypted == Some(true);
    state.test_running = true;
    set_enabled(state.sync.test, false);
    let result_slot = Arc::clone(&state.test_result);
    let hwnd_value = util::hwnd_to_isize(state.hwnd);
    let spawned = std::thread::Builder::new()
        .name("vibekey-test".to_string())
        .spawn(move || {
            let (ok, message) = match UreqWebDav::new_with_fingerprint(
                &username,
                &password,
                Duration::from_secs(timeout),
                &certificate_sha256,
            ) {
                Err(error) => (false, format!("证书指纹配置无效：{error}")),
                Ok(dav) => match dav.get(&url) {
                    Ok(Some(remote)) if remote.encrypted => encryption_test_result(
                        &remote.body,
                        &encryption_passwords,
                        encryption_enabled,
                        disable_encryption_allowed,
                    ),
                    Ok(Some(remote)) => (
                        true,
                        if encryption_enabled {
                            format!(
                                "连接成功；远端当前是 {} 字节的明文配置，下次同步会加密迁移。",
                                remote.body.len()
                            )
                        } else {
                            format!("连接成功，远端配置文件 {} 字节。", remote.body.len())
                        },
                    ),
                    Ok(None) => (
                        true,
                        if encryption_enabled {
                            "连接成功，远端文件尚不存在；首次同步会加密上传。".to_string()
                        } else {
                            "连接成功，但远端文件还不存在（首次同步时会上传）。".to_string()
                        },
                    ),
                    Err(error) => (false, format!("连接失败：{error}")),
                },
            };
            if let Ok(mut slot) = result_slot.lock() {
                *slot = Some((ok, message));
            }
            // SAFETY: posting to an already-destroyed HWND simply fails.
            unsafe {
                PostMessageW(util::isize_to_hwnd(hwnd_value), WM_APP_TEST_RESULT, 0, 0);
            }
        });
    if spawned.is_err() {
        state.test_running = false;
        set_enabled(state.sync.test, true);
        util::message_box_error(state.hwnd, "无法启动测试线程。", "测试连接");
    }
}

fn encryption_test_result(
    body: &[u8],
    passwords: &[String],
    encryption_enabled: bool,
    disable_encryption_allowed: bool,
) -> (bool, String) {
    if !encryption_enabled && !disable_encryption_allowed {
        return (
            false,
            "远端已加密；请先勾选配置加密并成功同步，之后才能主动关闭加密。".to_string(),
        );
    }
    if passwords.is_empty() {
        return (
            false,
            "连接成功，但远端配置已加密；请填写独立同步加密密码。".to_string(),
        );
    }
    for (index, password) in passwords.iter().enumerate() {
        match decrypt_config(body, password) {
            Ok(plain) => {
                let used_previous = index > 0;
                let message = match (used_previous, encryption_enabled) {
                    (false, true) => format!(
                        "连接及当前加密密码验证成功；远端密文 {} 字节，解密后 {} 字节。",
                        body.len(),
                        plain.len()
                    ),
                    (true, true) => format!(
                        "连接成功；旧密码可解密，保存并同步后即可完成密码轮换（解密后 {} 字节）。",
                        plain.len()
                    ),
                    (false, false) => format!(
                        "连接及密码验证成功；保存并同步后可迁移为明文（解密后 {} 字节）。",
                        plain.len()
                    ),
                    (true, false) => format!(
                        "连接成功；旧密码可解密，保存并同步后可关闭配置加密（解密后 {} 字节）。",
                        plain.len()
                    ),
                };
                return (true, message);
            }
            Err(CryptoError::InvalidFormat) => {
                return (
                    false,
                    "连接成功，但远端加密格式无效或版本不受支持。".to_string(),
                );
            }
            Err(_) => {
                // GCM intentionally cannot distinguish a wrong password from
                // tampering. Try the one-time rotation fallback, if present.
            }
        }
    }
    (
        false,
        "连接成功，但加密密码错误，或远端内容已被篡改。".to_string(),
    )
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}
