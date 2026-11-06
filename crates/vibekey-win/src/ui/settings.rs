//! The settings window.
//!
//! All controls are created programmatically (no dialog resources) so that the
//! binary stays self contained. Tab navigation, the default button (Enter) and
//! Escape are provided by IsDialogMessageW, which the main message loop calls
//! for this window.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vibekey_core::sync::{UreqWebDav, WebDav};
use vibekey_core::{ActivationMode, Config, InputMethod, SecretStore};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetStockObject, UpdateWindow, COLOR_BTNFACE, DEFAULT_GUI_FONT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::BST_CHECKED;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::autostart;
use crate::util::{self, wide, WM_APP_TEST_RESULT};

const CLASS_NAME: &str = "VibekeySettings";
const CLIENT_WIDTH: i32 = 460;
const CLIENT_HEIGHT: i32 = 384;

const LABEL_HEIGHT: i32 = 18;
const EDIT_HEIGHT: i32 = 22;
const BUTTON_HEIGHT: i32 = 26;

const ID_SYNC_ENABLED: i32 = 100;
const ID_URL: i32 = 101;
const ID_USER: i32 = 102;
const ID_PASSWORD: i32 = 103;
const ID_INTERVAL: i32 = 104;
const ID_SYNC_ON_CHANGE: i32 = 105;
const ID_BACKUP: i32 = 106;
const ID_INPUT_METHOD: i32 = 107;
const ID_ACTIVATION: i32 = 108;
const ID_AUTOSTART: i32 = 109;
const ID_TEST: i32 = 110;

/// ID of the default button; IsDialogMessageW sends WM_COMMAND with this id for
/// Enter.
const ID_SAVE: i32 = IDOK;
const ID_CANCEL: i32 = IDCANCEL;

#[derive(Default, Clone, Copy)]
struct Controls {
    sync_enabled: HWND,
    url: HWND,
    user: HWND,
    password: HWND,
    interval: HWND,
    sync_on_change: HWND,
    backup: HWND,
    input_method: HWND,
    activation: HWND,
    autostart: HWND,
    test: HWND,
    save: HWND,
    cancel: HWND,
    password_state: HWND,
}

struct SettingsState {
    hwnd: HWND,
    controls: Controls,
    config: Config,
    config_path: PathBuf,
    secrets: Arc<dyn SecretStore>,
    main_hwnd: HWND,
    test_result: Arc<Mutex<Option<(bool, String)>>>,
    test_running: bool,
}

static REGISTERED: AtomicBool = AtomicBool::new(false);

/// Open the settings window, or bring the existing one to the front.
pub fn open(
    main_hwnd: HWND,
    config: &Config,
    config_path: &Path,
    secrets: Arc<dyn SecretStore>,
) -> Option<HWND> {
    if !ensure_class() {
        log::error!("could not register the settings window class");
        return None;
    }
    let scale = util::dpi_scale();
    let style = WS_OVERLAPPEDWINDOW & !WS_MAXIMIZEBOX & !WS_THICKFRAME;
    let mut rect = util::rect(
        0,
        0,
        (CLIENT_WIDTH as f32 * scale) as i32,
        (CLIENT_HEIGHT as f32 * scale) as i32,
    );
    // SAFETY: rect is a valid RECT that is adjusted in place.
    unsafe {
        AdjustWindowRect(&mut rect, style, 0);
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    // SAFETY: GetSystemMetrics has no preconditions.
    let (screen_width, screen_height) =
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let x = ((screen_width - width) / 2).max(0);
    let y = ((screen_height - height) / 2).max(0);

    let class_name = wide(CLASS_NAME);
    let title = wide("Vibekey 设置");
    // SAFETY: the class is registered and the string buffers live for the
    // duration of the call.
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
        controls: Controls::default(),
        config: config.clone(),
        config_path: config_path.to_path_buf(),
        secrets,
        main_hwnd,
        test_result: Arc::new(Mutex::new(None)),
        test_running: false,
    });
    let pointer = Box::into_raw(state);
    // SAFETY: the pointer is owned by the window until WM_NCDESTROY; the window
    // is live.
    unsafe {
        util::set_window_userdata(hwnd, pointer as isize);
        create_controls(&mut *pointer);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetForegroundWindow(hwnd);
        // Give the first control the focus so that Tab navigation and typing
        // work right away.
        SetFocus((*pointer).controls.sync_enabled);
    }
    Some(hwnd)
}

fn ensure_class() -> bool {
    if REGISTERED.load(Ordering::Acquire) {
        return true;
    }
    // SAFETY: the class name buffer lives for the duration of the call.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide(CLASS_NAME);
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(settings_proc);
        class.hInstance = instance;
        class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        class.hbrBackground = (COLOR_BTNFACE + 1) as usize as *mut core::ffi::c_void;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return false;
        }
    }
    REGISTERED.store(true, Ordering::Release);
    true
}

/// Create one child control with the default GUI font.
unsafe fn create_control(
    parent: HWND,
    class_name: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: i32,
    font: *mut core::ffi::c_void,
) -> HWND {
    let class_wide = wide(class_name);
    let text_wide = wide(text);
    let instance = GetModuleHandleW(std::ptr::null());
    let hwnd = CreateWindowExW(
        0,
        class_wide.as_ptr(),
        text_wide.as_ptr(),
        style | WS_CHILD | WS_VISIBLE,
        x,
        y,
        width,
        height,
        parent,
        id as usize as *mut core::ffi::c_void,
        instance,
        std::ptr::null(),
    );
    if !hwnd.is_null() && !font.is_null() {
        SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
    }
    hwnd
}

unsafe fn create_controls(state: &mut SettingsState) {
    let hwnd = state.hwnd;
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let x = util::scale;
    let config = state.config.clone();

    let label = |text: &str, left: i32, top: i32, width: i32| {
        create_control(
            hwnd,
            "STATIC",
            text,
            0,
            x(left),
            x(top),
            x(width),
            x(LABEL_HEIGHT),
            -1,
            font,
        )
    };
    let edit = |text: &str, left: i32, top: i32, width: i32, style: i32, id: i32| {
        create_control(
            hwnd,
            "EDIT",
            text,
            (ES_LEFT | ES_AUTOHSCROLL | style) as u32 | WS_TABSTOP,
            x(left),
            x(top),
            x(width),
            x(EDIT_HEIGHT),
            id,
            font,
        )
    };
    let checkbox = |text: &str, left: i32, top: i32, width: i32, checked: bool, id: i32| {
        let control = create_control(
            hwnd,
            "BUTTON",
            text,
            BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
            x(left),
            x(top),
            x(width),
            x(20),
            id,
            font,
        );
        if checked {
            SendMessageW(control, BM_SETCHECK, BST_CHECKED as usize, 0);
        }
        control
    };

    label("同步", 12, 10, 100);
    state.controls.sync_enabled = checkbox(
        "启用 WebDAV 同步",
        12,
        30,
        300,
        config.sync.enabled,
        ID_SYNC_ENABLED,
    );
    label("WebDAV 地址", 12, 60, 100);
    state.controls.url = edit(&config.sync.url, 124, 58, 320, 0, ID_URL);
    label("用户名", 12, 88, 100);
    state.controls.user = edit(&config.sync.username, 124, 86, 200, 0, ID_USER);
    label("密码", 12, 116, 100);
    state.controls.password = edit("", 124, 114, 200, ES_PASSWORD, ID_PASSWORD);
    state.controls.password_state = label("", 334, 116, 110);
    label("同步间隔（分钟）", 12, 144, 120);
    state.controls.interval = edit(
        &config.sync.interval_minutes.to_string(),
        124,
        142,
        80,
        0,
        ID_INTERVAL,
    );
    state.controls.sync_on_change = checkbox(
        "保存配置后立即同步",
        12,
        172,
        300,
        config.sync.sync_on_change,
        ID_SYNC_ON_CHANGE,
    );
    state.controls.backup = checkbox(
        "冲突时备份远端配置",
        12,
        196,
        300,
        config.sync.backup_on_conflict,
        ID_BACKUP,
    );

    label("行为", 12, 228, 100);
    label("输入方式", 12, 252, 100);
    state.controls.input_method = create_control(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_VSCROLL | WS_TABSTOP,
        x(124),
        x(250),
        x(200),
        x(200),
        ID_INPUT_METHOD,
        font,
    );
    let paste_text = wide("剪贴板粘贴（推荐，中文最稳）");
    SendMessageW(
        state.controls.input_method,
        CB_ADDSTRING,
        0,
        paste_text.as_ptr() as isize,
    );
    let type_text = wide("直接输入（SendInput Unicode）");
    SendMessageW(
        state.controls.input_method,
        CB_ADDSTRING,
        0,
        type_text.as_ptr() as isize,
    );
    let input_index = match config.input_method {
        InputMethod::Paste => 0,
        InputMethod::Type => 1,
    };
    SendMessageW(state.controls.input_method, CB_SETCURSEL, input_index, 0);

    label("触发方式", 12, 280, 100);
    state.controls.activation = create_control(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_VSCROLL | WS_TABSTOP,
        x(124),
        x(278),
        x(200),
        x(200),
        ID_ACTIVATION,
        font,
    );
    let immediate_text = wide("立即触发（Alt+数字）");
    SendMessageW(
        state.controls.activation,
        CB_ADDSTRING,
        0,
        immediate_text.as_ptr() as isize,
    );
    let hold_text = wide("按住 Alt 后再触发");
    SendMessageW(
        state.controls.activation,
        CB_ADDSTRING,
        0,
        hold_text.as_ptr() as isize,
    );
    let activation_index = match config.activation {
        ActivationMode::Immediate => 0,
        ActivationMode::Hold => 1,
    };
    SendMessageW(state.controls.activation, CB_SETCURSEL, activation_index, 0);

    state.controls.autostart = checkbox(
        "开机自动启动",
        12,
        310,
        300,
        config.start_with_windows,
        ID_AUTOSTART,
    );

    state.controls.test = create_control(
        hwnd,
        "BUTTON",
        "测试连接",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        x(12),
        x(344),
        x(100),
        x(BUTTON_HEIGHT),
        ID_TEST,
        font,
    );
    state.controls.save = create_control(
        hwnd,
        "BUTTON",
        "保存",
        BS_DEFPUSHBUTTON as u32 | WS_TABSTOP,
        x(250),
        x(344),
        x(90),
        x(BUTTON_HEIGHT),
        ID_SAVE,
        font,
    );
    state.controls.cancel = create_control(
        hwnd,
        "BUTTON",
        "取消",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        x(348),
        x(344),
        x(90),
        x(BUTTON_HEIGHT),
        ID_CANCEL,
        font,
    );

    update_password_state(state);
}

fn update_password_state(state: &mut SettingsState) {
    let message = match state.secrets.load_password() {
        Ok(Some(_)) => "已保存（留空不修改）",
        _ => "未保存",
    };
    // SAFETY: the label handle is a live child window.
    unsafe {
        let text = wide(message);
        SetWindowTextW(state.controls.password_state, text.as_ptr());
    }
}

fn window_text(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return String::new();
    }
    let mut buffer = [0u16; 4096];
    // SAFETY: the buffer and its length are valid; the window is a live child.
    let length = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        return String::new();
    }
    util::from_wide(&buffer[..length as usize])
}

fn checked(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    // SAFETY: the handle is a live check box.
    unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
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

unsafe extern "system" fn settings_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The user data pointer is only turned into a reference inside the branches
    // that need it, so that WM_NCDESTROY can release the box without keeping a
    // reference to it alive.
    let pointer = util::window_userdata(hwnd) as *mut SettingsState;

    match message {
        WM_COMMAND => {
            if !pointer.is_null() {
                let state = &mut *pointer;
                match (wparam & 0xFFFF) as i32 {
                    ID_SAVE => {
                        if save(state) {
                            DestroyWindow(hwnd);
                        }
                    }
                    ID_CANCEL => {
                        DestroyWindow(hwnd);
                    }
                    ID_TEST => test_connection(state),
                    _ => {}
                }
            }
            0
        }
        WM_APP_TEST_RESULT => {
            if !pointer.is_null() {
                let state = &mut *pointer;
                state.test_running = false;
                EnableWindow(state.controls.test, 1);
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
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            crate::app::app_mut().settings = std::ptr::null_mut();
            0
        }
        WM_NCDESTROY => {
            if !pointer.is_null() {
                util::set_window_userdata(hwnd, 0);
                // SAFETY: the box was created with Box::into_raw in open() and
                // is released exactly once here.
                drop(Box::from_raw(pointer));
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Read the controls, persist the configuration and the password. Returns true
/// when the window can be closed.
fn save(state: &mut SettingsState) -> bool {
    let mut config = state.config.clone();
    config.sync.enabled = checked(state.controls.sync_enabled);
    config.sync.url = window_text(state.controls.url).trim().to_string();
    config.sync.username = window_text(state.controls.user).trim().to_string();
    config.sync.sync_on_change = checked(state.controls.sync_on_change);
    config.sync.backup_on_conflict = checked(state.controls.backup);
    config.start_with_windows = checked(state.controls.autostart);
    config.input_method = if combo_selection(state.controls.input_method) == 0 {
        InputMethod::Paste
    } else {
        InputMethod::Type
    };
    config.activation = if combo_selection(state.controls.activation) == 0 {
        ActivationMode::Immediate
    } else {
        ActivationMode::Hold
    };

    let interval_text = window_text(state.controls.interval).trim().to_string();
    match interval_text.parse::<u32>() {
        Ok(minutes) => config.sync.interval_minutes = minutes,
        Err(_) => {
            util::message_box_error(
                state.hwnd,
                "同步间隔必须是数字（分钟，0 表示只手动同步）。",
                "保存失败",
            );
            return false;
        }
    }

    if config.sync.enabled {
        if config.sync.url.is_empty() {
            util::message_box_error(state.hwnd, "启用同步时必须填写 WebDAV 地址。", "保存失败");
            return false;
        }
        if !(config.sync.url.starts_with("http://") || config.sync.url.starts_with("https://")) {
            util::message_box_error(
                state.hwnd,
                "WebDAV 地址必须以 http:// 或 https:// 开头。",
                "保存失败",
            );
            return false;
        }
    }

    let password = window_text(state.controls.password);
    if !password.is_empty() {
        if let Err(error) = state.secrets.store_password(&password) {
            util::message_box_error(state.hwnd, &format!("无法保存密码：{error}"), "保存失败");
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

    match autostart::set_enabled(config.start_with_windows) {
        Ok(()) => {}
        Err(error) => {
            util::message_box_warning(state.hwnd, &format!("开机启动设置失败：{error}"), "保存");
        }
    }

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
    let url = window_text(state.controls.url).trim().to_string();
    if url.is_empty() {
        util::message_box_warning(state.hwnd, "请先填写 WebDAV 地址。", "测试连接");
        return;
    }
    let username = window_text(state.controls.user).trim().to_string();
    let entered_password = window_text(state.controls.password);
    let password = if entered_password.is_empty() {
        match state.secrets.load_password() {
            Ok(Some(saved)) => saved,
            Ok(None) => {
                util::message_box_warning(state.hwnd, "请先填写密码。", "测试连接");
                return;
            }
            Err(error) => {
                util::message_box_error(
                    state.hwnd,
                    &format!("无法读取已保存的密码：{error}"),
                    "测试连接",
                );
                return;
            }
        }
    } else {
        entered_password
    };

    state.test_running = true;
    // SAFETY: the button is a live child window.
    unsafe {
        EnableWindow(state.controls.test, 0);
    }
    let result_slot = Arc::clone(&state.test_result);
    // HWND is not Send; pass the raw handle value through the thread closure.
    let hwnd_value = util::hwnd_to_isize(state.hwnd);
    let spawned = std::thread::Builder::new()
        .name("vibekey-test".to_string())
        .spawn(move || {
            let dav = UreqWebDav::new(&username, &password, Duration::from_secs(10));
            let (ok, message) = match dav.get(&url) {
                Ok(Some(remote)) => (
                    true,
                    format!("连接成功，远端配置文件 {} 字节。", remote.body.len()),
                ),
                Ok(None) => (
                    true,
                    "连接成功，但远端文件还不存在（首次同步时会上传）。".to_string(),
                ),
                Err(error) => (false, format!("连接失败：{error}")),
            };
            if let Ok(mut slot) = result_slot.lock() {
                *slot = Some((ok, message));
            }
            // SAFETY: the window handle was captured before the thread started;
            // PostMessageW simply fails when the window is gone.
            unsafe {
                PostMessageW(util::isize_to_hwnd(hwnd_value), WM_APP_TEST_RESULT, 0, 0);
            }
        });
    if spawned.is_err() {
        state.test_running = false;
        // SAFETY: the button is a live child window.
        unsafe {
            EnableWindow(state.controls.test, 1);
        }
        util::message_box_error(state.hwnd, "无法启动测试线程。", "测试连接");
    }
}
