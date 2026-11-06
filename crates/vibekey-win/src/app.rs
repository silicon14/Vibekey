//! Application state, the hidden main window and the message loop.
//!
//! WndProc functions cannot carry user data, so the single App instance lives in
//! a box whose address is published in the APP atomic pointer. Every message
//! handler runs on the main thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use vibekey_core::{expand, Config, ExpandContext, Keymap, Snippet};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::autostart;
use crate::hook::{self, HookConfig, HookHandle};
use crate::inject::{self, InjectHandle, InjectJob};
use crate::secret::DpapiSecretStore;
use crate::sync::{self, SyncHandle, SyncSettings};
use crate::ui::menu::{self, MenuState};
use crate::ui::popup::Popup;
use crate::ui::settings;
use crate::ui::tray::Tray;
use crate::util::{
    self, hwnd_to_isize, wide, ACTION_CONFIG, ACTION_LIST, ACTION_SNIPPET, TIMER_CLIP_RESTORE,
    TIMER_CONFIG_POLL, TIMER_TRAY_CLICK, WM_APP_ACTION, WM_APP_CLIP_RESTORE, WM_APP_INJECT_DONE,
    WM_APP_RELOAD, WM_APP_SYNC_DONE, WM_APP_SYNC_NOW, WM_APP_TRAY,
};

/// Time window in which the same notification category is suppressed.
const NOTIFY_SUPPRESS: Duration = Duration::from_secs(600);

/// Application state.
pub struct App {
    pub hwnd: HWND,
    pub config_path: PathBuf,
    pub config: Config,
    pub keymap: Option<Keymap>,
    secrets: Arc<dyn vibekey_core::SecretStore>,
    tray: Option<Tray>,
    hook: Option<HookHandle>,
    inject: Option<InjectHandle>,
    sync: Option<SyncHandle>,
    /// The snippet picker; messages of its window are handled in ui::popup.
    pub popup: Popup,
    /// Settings window handle, or null.
    pub settings: HWND,
    last_mtime: Option<SystemTime>,
    last_notify: HashMap<&'static str, Instant>,
    sync_in_progress: bool,
    taskbar_created: u32,
    /// A tray single click is waiting for the double click time to pass.
    tray_click_pending: bool,
    /// The second click of a tray double click arrived; ignore its button up.
    tray_double_click_seen: bool,
    quitting: bool,
    /// Kept alive for the whole process; dropping it would release the single
    /// instance mutex.
    _mutex: HANDLE,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

/// Access the single application instance.
///
/// SAFETY: must only be called on the main thread while the App is alive, which
/// is the case for every window procedure owned by this crate.
pub unsafe fn app_mut<'a>() -> &'a mut App {
    let pointer = APP.load(Ordering::Acquire);
    debug_assert!(!pointer.is_null(), "app accessed before initialisation");
    &mut *pointer
}

/// Create the mutex used for single instance detection. Returns the handle and
/// whether another instance already owns the mutex.
fn acquire_instance_mutex() -> (HANDLE, bool) {
    let name = wide("Local\\Vibekey.SingleInstance");
    // SAFETY: the name is a valid NUL terminated string; a null security
    // attribute pointer means the default descriptor.
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if mutex.is_null() {
        log::warn!("could not create the single instance mutex");
        return (mutex, false);
    }
    // SAFETY: GetLastError has no preconditions and is read right after the
    // owning call, before any other Windows API runs.
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    (mutex, already_running)
}

/// Entry point of the application. Returns the process exit code.
pub fn run() -> i32 {
    // SAFETY: SetProcessDPIAware must be called before any window is created.
    unsafe {
        SetProcessDPIAware();
    }

    let config_dir = Config::config_dir();
    crate::log::init(&config_dir);

    let (mutex, already_running) = acquire_instance_mutex();
    if already_running {
        log::info!("another Vibekey instance is already running");
        return 0;
    }

    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
    }));

    let pointer = Box::into_raw(Box::new(App::new(config_dir, mutex)));
    APP.store(pointer, Ordering::Release);
    // SAFETY: the pointer was just created and is uniquely owned here.
    let app = unsafe { &mut *pointer };

    if !app.create_window() {
        log::error!("could not create the main window");
        APP.store(std::ptr::null_mut(), Ordering::Release);
        // SAFETY: the box was created above and is released exactly once.
        unsafe {
            drop(Box::from_raw(pointer));
        }
        return 1;
    }
    app.start();
    let exit_code = app.message_loop();

    APP.store(std::ptr::null_mut(), Ordering::Release);
    // SAFETY: the box is released exactly once; shutdown is idempotent.
    unsafe {
        let mut app = Box::from_raw(pointer);
        app.shutdown();
        drop(app);
    }
    exit_code
}

impl App {
    fn new(config_dir: PathBuf, mutex: HANDLE) -> Self {
        Self {
            hwnd: std::ptr::null_mut(),
            config_path: Config::default_path(),
            config: Config::default(),
            keymap: None,
            secrets: Arc::new(DpapiSecretStore::new(config_dir.join("secret.bin"))),
            tray: None,
            hook: None,
            inject: None,
            sync: None,
            popup: Popup::new(),
            settings: std::ptr::null_mut(),
            last_mtime: None,
            last_notify: HashMap::new(),
            sync_in_progress: false,
            taskbar_created: 0,
            tray_click_pending: false,
            tray_double_click_seen: false,
            quitting: false,
            _mutex: mutex,
        }
    }

    fn create_window(&mut self) -> bool {
        // SAFETY: the class name and title buffers live for the duration of the
        // calls; RegisterClassW copies the strings.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name = wide("VibekeyMainWnd");
            let mut class: WNDCLASSW = std::mem::zeroed();
            class.lpfnWndProc = Some(main_window_proc);
            class.hInstance = instance;
            class.lpszClassName = class_name.as_ptr();
            if RegisterClassW(&class) == 0 {
                let error = GetLastError();
                log::error!("RegisterClassW failed (error {error})");
                return false;
            }
            let title = wide("Vibekey");
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let error = GetLastError();
                log::error!("CreateWindowExW failed (error {error})");
                return false;
            }
            self.hwnd = hwnd;
        }
        true
    }

    /// Load the configuration and start the tray icon, the hook and the worker
    /// threads. Runs after the main window exists.
    fn start(&mut self) {
        // SAFETY: the message string is a valid NUL terminated buffer.
        self.taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };

        self.load_initial_config();
        self.tray = Some(Tray::add(self.hwnd));

        match hook::start(self.hwnd) {
            Some(handle) => {
                handle
                    .shared()
                    .set_config(HookConfig::from_config(&self.config, self.keymap.clone()));
                self.hook = Some(handle);
            }
            None => {
                self.notify("hook", "无法安装键盘钩子，快捷键不可用（详见日志）", true);
            }
        }

        self.inject = Some(inject::start(self.hwnd));

        let sync = sync::start(self.hwnd, Arc::clone(&self.secrets));
        sync.update(SyncSettings::from_config(
            &self.config,
            self.config_path.clone(),
        ));
        self.sync = Some(sync);

        // SAFETY: the window handle is live and the timer id is ours.
        unsafe {
            SetTimer(self.hwnd, TIMER_CONFIG_POLL, 1000, None);
        }

        autostart::reconcile(self.config.start_with_windows);
        log::info!(
            "started; {} snippets, configuration at {}",
            self.config.snippet.len(),
            self.config_path.display()
        );
    }

    fn load_initial_config(&mut self) {
        match Config::load_or_create(&self.config_path) {
            Ok(config) => self.config = config,
            Err(error) => {
                log::error!("could not load the configuration: {error}");
                self.config = Config::default();
                util::message_box_warning(
                    self.hwnd,
                    &format!(
                        "读取配置失败，已使用默认配置（不会覆盖原文件）。\n\n{}\n\n{error}",
                        self.config_path.display()
                    ),
                    "Vibekey",
                );
            }
        }
        self.rebuild_keymap(false);
        self.last_mtime = file_mtime(&self.config_path);
    }

    fn rebuild_keymap(&mut self, report: bool) {
        for issue in self.config.validate() {
            log::warn!("configuration: {issue}");
        }
        match Keymap::build(&self.config) {
            Ok(keymap) => self.keymap = Some(keymap),
            Err(errors) => {
                let text = errors
                    .iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                log::error!("invalid hotkeys: {text}");
                self.keymap = None;
                if report {
                    self.notify(
                        "hotkey",
                        &format!("快捷键配置有误，快捷键已禁用：{text}"),
                        true,
                    );
                }
            }
        }
    }

    /// Reload the configuration file; a broken file keeps the old config.
    fn reload_config(&mut self, manual: bool) {
        match Config::load(&self.config_path) {
            Ok(config) => {
                self.config = config;
                // Report keymap problems both for manual and automatic reloads;
                // the notification is throttled per category.
                self.rebuild_keymap(true);
                self.publish_config();
                self.last_mtime = file_mtime(&self.config_path);
                log::info!("configuration reloaded");
                if manual {
                    self.notify_always("配置已重新加载", false);
                }
            }
            Err(error) => {
                log::warn!("configuration reload failed: {error}");
                self.last_mtime = file_mtime(&self.config_path);
                self.notify(
                    "reload",
                    &format!("配置读取失败，继续使用旧配置：{error}"),
                    true,
                );
            }
        }
    }

    /// Push the current configuration to the hook and sync threads.
    fn publish_config(&mut self) {
        if let Some(handle) = self.hook.as_ref() {
            handle
                .shared()
                .set_config(HookConfig::from_config(&self.config, self.keymap.clone()));
        }
        if let Some(sync) = self.sync.as_ref() {
            sync.update(SyncSettings::from_config(
                &self.config,
                self.config_path.clone(),
            ));
        }
    }

    fn notify(&mut self, category: &'static str, text: &str, warning: bool) {
        let now = Instant::now();
        if let Some(last) = self.last_notify.get(category) {
            if now.duration_since(*last) < NOTIFY_SUPPRESS {
                return;
            }
        }
        self.last_notify.insert(category, now);
        self.notify_always(text, warning);
    }

    fn notify_always(&self, text: &str, warning: bool) {
        if let Some(tray) = self.tray.as_ref() {
            tray.notify(text, warning);
        }
    }

    /// Expand a snippet and queue it for injection.
    pub fn inject_snippet(&mut self, index: usize, target: HWND) {
        let Some(snippet) = self.config.snippet.get(index) else {
            return;
        };
        if !snippet.enabled {
            return;
        }
        let clipboard_text = crate::clipboard::read_text();
        let now = chrono::Local::now();
        let user = std::env::var("USERNAME").unwrap_or_default();
        let host = std::env::var("COMPUTERNAME").unwrap_or_default();
        let context = ExpandContext {
            now,
            clipboard: clipboard_text.as_deref(),
            user: &user,
            host: &host,
        };
        let text = expand(&snippet.text, &context);
        let send_enter = snippet.send_enter;
        let target = if target.is_null() {
            // SAFETY: GetForegroundWindow has no preconditions.
            unsafe { GetForegroundWindow() }
        } else {
            target
        };
        let job = InjectJob {
            text,
            send_enter,
            target: hwnd_to_isize(target),
            input_method: self.config.input_method,
            restore_clipboard: self.config.restore_clipboard,
        };
        let submitted = self
            .inject
            .as_ref()
            .map(|handle| handle.submit(job))
            .unwrap_or(false);
        if !submitted {
            self.notify("inject", "无法启动输入线程，注入失败", true);
        }
    }

    /// Open (or close) the snippet picker.
    pub fn open_list(&mut self) {
        if self.popup.is_open() {
            self.popup.hide();
            return;
        }
        let items: Vec<usize> = self
            .config
            .snippet
            .iter()
            .enumerate()
            .filter(|(_, snippet)| snippet.enabled)
            .map(|(index, _)| index)
            .collect();
        if items.is_empty() {
            self.notify_always("没有可用的片段", true);
            return;
        }
        let texts: Vec<String> = items
            .iter()
            .map(|&index| snippet_display_text(&self.config.snippet[index]))
            .collect();
        let mut target = usable_foreground_window();
        if target.is_null() {
            // Opened from the tray icon: the shell owns the foreground window,
            // so fall back to the last window that received a key press.
            if let Some(handle) = self.hook.as_ref() {
                let candidate = handle.shared().last_foreground();
                // SAFETY: IsWindow accepts any handle and only reads.
                if !candidate.is_null() && unsafe { IsWindow(candidate) } != 0 {
                    target = candidate;
                }
            }
        }
        if !self.popup.show(self.hwnd, items, &texts, target) {
            self.notify("popup", "无法打开片段列表", true);
        }
    }

    fn show_menu(&mut self, point: POINT) {
        let state = MenuState {
            sync_available: self.config.sync.enabled,
            syncing: self.sync_in_progress,
            autostart: self.config.start_with_windows,
        };
        if let Some(command) = menu::show(self.hwnd, point, &state) {
            self.handle_menu_command(command);
        }
    }

    fn handle_menu_command(&mut self, command: u32) {
        match command {
            menu::CMD_LIST => self.open_list(),
            menu::CMD_EDIT_CONFIG => {
                let path = self.config_path.clone();
                self.open_in_editor(&path);
            }
            menu::CMD_RELOAD => self.reload_config(true),
            menu::CMD_SYNC => self.request_sync(true),
            menu::CMD_SETTINGS => self.open_settings(),
            menu::CMD_AUTOSTART => self.toggle_autostart(),
            menu::CMD_OPEN_LOG => {
                let path = crate::log::log_path(&Config::config_dir());
                self.open_in_editor(&path);
            }
            menu::CMD_ABOUT => self.show_about(),
            menu::CMD_EXIT => {
                // SAFETY: the main window handle is live.
                unsafe {
                    DestroyWindow(self.hwnd);
                }
            }
            _ => {}
        }
    }

    fn open_in_editor(&mut self, path: &Path) {
        match std::process::Command::new("notepad.exe").arg(path).spawn() {
            Ok(_) => {}
            Err(error) => {
                log::warn!("could not start notepad: {error}");
                self.notify("editor", &format!("无法打开编辑器：{error}"), true);
            }
        }
    }

    fn toggle_autostart(&mut self) {
        let enabled = !self.config.start_with_windows;
        match autostart::set_enabled(enabled) {
            Ok(()) => {
                self.config.start_with_windows = enabled;
                if let Err(error) = self.config.save(&self.config_path) {
                    log::warn!("could not save the configuration: {error}");
                    self.notify("save", &format!("无法保存配置：{error}"), true);
                }
                self.last_mtime = file_mtime(&self.config_path);
                self.notify_always(
                    if enabled {
                        "已启用开机自动启动"
                    } else {
                        "已关闭开机自动启动"
                    },
                    false,
                );
            }
            Err(error) => self.notify("autostart", &format!("开机启动设置失败：{error}"), true),
        }
    }

    fn open_settings(&mut self) {
        // SAFETY: IsWindow accepts any handle.
        if !self.settings.is_null() && unsafe { IsWindow(self.settings) } != 0 {
            // SAFETY: the handle is a live window.
            unsafe {
                SetForegroundWindow(self.settings);
            }
            return;
        }
        self.settings = settings::open(
            self.hwnd,
            &self.config,
            &self.config_path,
            Arc::clone(&self.secrets),
        )
        .unwrap_or(std::ptr::null_mut());
    }

    fn show_about(&self) {
        let hotkeys = match self.keymap.as_ref() {
            Some(keymap) => format!(
                "片段列表：{}\n配置菜单：{}",
                keymap.list_hotkey().display(),
                keymap.config_hotkey().display()
            ),
            None => format!(
                "片段列表：{}\n配置菜单：{}",
                self.config.hotkeys.list, self.config.hotkeys.config
            ),
        };
        let text = format!(
            "Vibekey {}\n\n{}\n\n配置文件：{}\n\n按住 Alt 按数字键即可插入片段；Alt+反引号打开片段列表。\n支持 Windows 7 SP1 及更新版本。",
            env!("CARGO_PKG_VERSION"),
            hotkeys,
            self.config_path.display()
        );
        util::message_box_info(self.hwnd, &text, "关于 Vibekey");
    }

    fn request_sync(&mut self, manual: bool) {
        if !self.config.sync.enabled {
            if manual {
                self.notify_always("同步未启用，请先在设置中配置 WebDAV", true);
            }
            return;
        }
        if self.sync_in_progress && manual {
            self.notify_always("同步正在进行中", false);
            return;
        }
        self.sync_in_progress = true;
        match self.sync.as_ref() {
            Some(sync) => sync.sync_now(manual),
            None => self.sync_in_progress = false,
        }
    }

    fn restore_clipboard(&mut self) {
        let pending = self
            .inject
            .as_ref()
            .and_then(|handle| handle.shared().take_restore());
        if let Some(text) = pending {
            if !crate::clipboard::write_text(&text) {
                log::warn!("could not restore the clipboard");
            }
        }
    }

    fn poll_config(&mut self) {
        let current = file_mtime(&self.config_path);
        if current != self.last_mtime {
            self.last_mtime = current;
            self.reload_config(false);
        }
    }

    fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_CONFIG_POLL => self.poll_config(),
            TIMER_CLIP_RESTORE => {
                // SAFETY: the window handle is live and the timer id is ours.
                unsafe {
                    KillTimer(self.hwnd, TIMER_CLIP_RESTORE);
                }
                self.restore_clipboard();
            }
            TIMER_TRAY_CLICK => {
                // SAFETY: the window handle is live and the timer id is ours.
                unsafe {
                    KillTimer(self.hwnd, TIMER_TRAY_CLICK);
                }
                if self.tray_click_pending {
                    self.tray_click_pending = false;
                    self.open_list();
                }
            }
            _ => {}
        }
    }

    fn on_action(&mut self, action: usize, lparam: LPARAM) {
        match action {
            ACTION_LIST => self.open_list(),
            ACTION_CONFIG => {
                let point = util::caret_pos();
                self.show_menu(point);
            }
            ACTION_SNIPPET => {
                let trigger = self
                    .hook
                    .as_ref()
                    .map(|handle| handle.shared().take_trigger_hwnd())
                    .unwrap_or(std::ptr::null_mut());
                let target = if trigger.is_null() {
                    // SAFETY: GetForegroundWindow has no preconditions.
                    unsafe { GetForegroundWindow() }
                } else {
                    trigger
                };
                self.inject_snippet(lparam as usize, target);
            }
            _ => {}
        }
    }

    fn on_tray(&mut self, lparam: LPARAM) {
        let event = (lparam as u32) & 0xFFFF;
        match event {
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                let point = util::mouse_pos();
                self.show_menu(point);
            }
            WM_LBUTTONUP => {
                if self.tray_double_click_seen {
                    // The button up that belongs to the double click.
                    self.tray_double_click_seen = false;
                    return;
                }
                // Delay the single click so that a double click does not also
                // open the snippet list first.
                self.tray_click_pending = true;
                // SAFETY: the window handle is live and the timer id is ours.
                let delay = unsafe { GetDoubleClickTime() }.clamp(200, 1000);
                unsafe {
                    SetTimer(self.hwnd, TIMER_TRAY_CLICK, delay, None);
                }
            }
            WM_LBUTTONDBLCLK => {
                // SAFETY: the window handle is live and the timer id is ours.
                unsafe {
                    KillTimer(self.hwnd, TIMER_TRAY_CLICK);
                }
                self.tray_click_pending = false;
                self.tray_double_click_seen = true;
                self.open_settings();
            }
            _ => {}
        }
    }

    fn on_inject_done(&mut self, wparam: usize) {
        if wparam != 0 {
            let message = self
                .inject
                .as_ref()
                .and_then(|handle| handle.shared().take_error())
                .unwrap_or_else(|| "注入失败".to_string());
            self.notify("inject", &message, true);
        }
    }

    fn on_sync_done(&mut self) {
        self.sync_in_progress = false;
        let result = self
            .sync
            .as_ref()
            .and_then(|handle| handle.shared().take_result());
        let Some(result) = result else {
            return;
        };
        log::info!(
            "sync finished: ok={} pulled={} {}",
            result.ok,
            result.pulled,
            result.message
        );
        if result.ok {
            if result.manual || result.pulled {
                self.notify_always(&result.message, false);
            }
        } else if result.manual {
            self.notify_always(&result.message, true);
        } else {
            self.notify("sync", &result.message, true);
        }
    }

    fn message_loop(&mut self) -> i32 {
        let mut message: MSG = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: message is a valid out parameter; a null window filter
            // retrieves messages for the whole thread.
            let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
            if result == 0 || result == -1 {
                break;
            }
            let mut handled = false;
            let settings = self.settings;
            if !settings.is_null() && unsafe { IsWindow(settings) } != 0 {
                // SAFETY: the message is valid and the window is live; the call
                // only reads the message structure.
                handled = unsafe { IsDialogMessageW(settings, &message) } != 0;
            }
            if !handled {
                // SAFETY: the message was filled in by GetMessageW.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        message.wParam as i32
    }

    /// Remove the tray icon, stop the worker threads and end the message loop.
    fn shutdown(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        log::info!("shutting down");
        // SAFETY: the window handle is still live during WM_DESTROY.
        unsafe {
            KillTimer(self.hwnd, TIMER_CONFIG_POLL);
            KillTimer(self.hwnd, TIMER_CLIP_RESTORE);
            KillTimer(self.hwnd, TIMER_TRAY_CLICK);
        }
        if let Some(mut tray) = self.tray.take() {
            tray.remove();
        }
        if let Some(mut handle) = self.hook.take() {
            handle.stop();
        }
        if let Some(mut handle) = self.inject.take() {
            handle.stop();
        }
        if let Some(mut handle) = self.sync.take() {
            handle.stop();
        }
        // SAFETY: PostQuitMessage has no preconditions.
        unsafe {
            PostQuitMessage(0);
        }
    }
}

unsafe extern "system" fn main_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = APP.load(Ordering::Acquire);
    if pointer.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let app = &mut *pointer;

    if app.taskbar_created != 0 && message == app.taskbar_created {
        if let Some(tray) = app.tray.as_mut() {
            tray.re_add();
        }
        return 0;
    }

    match message {
        WM_TIMER => {
            app.on_timer(wparam);
            0
        }
        WM_APP_ACTION => {
            app.on_action(wparam, lparam);
            0
        }
        WM_APP_TRAY => {
            app.on_tray(lparam);
            0
        }
        WM_APP_INJECT_DONE => {
            app.on_inject_done(wparam);
            0
        }
        WM_APP_RELOAD => {
            app.reload_config(false);
            0
        }
        WM_APP_SYNC_NOW => {
            app.request_sync(true);
            0
        }
        WM_APP_SYNC_DONE => {
            app.on_sync_done();
            0
        }
        WM_APP_CLIP_RESTORE => {
            // Restore the clipboard a moment later, from the main thread, so
            // the paste in the target application has completed.
            SetTimer(
                hwnd,
                TIMER_CLIP_RESTORE,
                crate::inject::CLIPBOARD_RESTORE_DELAY_MS,
                None,
            );
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                app.shutdown();
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            app.shutdown();
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

/// Foreground window when it does not belong to this process, otherwise null.
fn usable_foreground_window() -> HWND {
    // SAFETY: GetForegroundWindow and GetWindowThreadProcessId have no
    // preconditions.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return std::ptr::null_mut();
        }
        let mut process_id = 0u32;
        GetWindowThreadProcessId(foreground, &mut process_id);
        if process_id == std::process::id() {
            std::ptr::null_mut()
        } else {
            foreground
        }
    }
}

/// List box label: "1. 名称 — 首行预览", "-." for snippets without a hotkey.
fn snippet_display_text(snippet: &Snippet) -> String {
    let key = if snippet.key.trim().is_empty() {
        "-".to_string()
    } else {
        snippet.key.trim().to_string()
    };
    let name = if snippet.name.trim().is_empty() {
        "（无名称）".to_string()
    } else {
        snippet.name.trim().to_string()
    };
    let first_line = snippet.text.lines().next().unwrap_or("").replace('\t', " ");
    let preview: String = first_line.chars().take(40).collect();
    format!("{key}. {name} — {preview}")
}
