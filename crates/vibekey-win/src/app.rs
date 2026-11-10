//! Application state, the hidden main window and the message loop.
//!
//! WndProc functions cannot carry user data, so the single App instance lives in
//! a box whose address is published in the APP atomic pointer.
//!
//! # Threading and aliasing contract
//!
//! Only the main (message loop) thread ever touches [App]: worker threads get
//! their own small shared states instead. Window procedures obtain a short lived
//! mutable reference per message from [try_app_mut] and tolerate `None` while
//! the process shuts down. [message_loop] itself only holds a raw pointer, so no
//! `&mut App` exists while [GetMessageW] blocks.
//!
//! Re-entrancy is expected: calling an API that pumps messages (MessageBoxW,
//! TrackPopupMenu, IsDialogMessageW, ...) may dispatch a new message to one of
//! our window procedures while the current handler is still on the stack. Each
//! nested call re-reads the state from the APP pointer instead of relying on a
//! cached reference, and no handler stores a raw `*mut App` between calls.
//!
//! Known residual caveat: a few handlers still hold their `&mut App` while they
//! call a pumping API (the "about" and settings validation dialogs through
//! MessageBoxW). Native menus are queued through WM_APP_SHOW_MENU instead. This is a
//! single threaded program and the handlers are written to be re-entrant, but
//! strictly speaking Rust's aliasing rules are only fully satisfied by an
//! interior mutability refactor (`RefCell`/`Cell` state) that is out of scope
//! for this fix. Do not add new code that keeps borrowed App fields alive across
//! such calls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use vibekey_core::{Config, Keymap};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, RemoveClipboardFormatListener,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::autostart;
use crate::history::{ClipboardHistory, HistoryControl};
use crate::hook::{self, HookConfig, HookHandle};
use crate::inject::{self, InjectHandle, InjectJob};
use crate::secret::DpapiSecretStore;
use crate::sync::{self, SyncHandle, SyncSettings};
use crate::ui::menu::{self, MenuState};
use crate::ui::popup::{PickerMode, Popup, PopupItem};
use crate::ui::settings;
use crate::ui::tray::Tray;
use crate::util::{
    self, hwnd_to_isize, wide, ACTION_CONFIG, ACTION_HISTORY, ACTION_LIST, ACTION_SNIPPET,
    TIMER_CONFIG_POLL, TIMER_TRAY_CLICK, WM_APP_ACTION, WM_APP_INJECT_DONE, WM_APP_RELOAD,
    WM_APP_SHOW_MENU, WM_APP_SYNC_DONE, WM_APP_SYNC_NOW, WM_APP_TRAY,
};

/// Time window in which the same notification category is suppressed.
const NOTIFY_SUPPRESS: Duration = Duration::from_secs(600);

/// Populate deterministic history rows for the isolated debug UI harness. This
/// message and handler are compiled out of every release build.
#[cfg(debug_assertions)]
const WM_APP_DEBUG_HISTORY: u32 = util::WM_APP + 20;

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
    /// The snippet/history picker; messages are handled in ui::popup.
    pub popup: Popup,
    /// Newest-first, memory-only text clipboard history.
    pub history: ClipboardHistory,
    /// Coordinates history capture with clipboard writes on the inject worker.
    history_control: Arc<HistoryControl>,
    clipboard_listener: bool,
    /// Settings window handle, or null.
    pub settings: HWND,
    last_mtime: Option<SystemTime>,
    last_notify: HashMap<&'static str, Instant>,
    sync_in_progress: bool,
    taskbar_created: u32,
    /// Screen position for the next WM_APP_SHOW_MENU message.
    pending_menu_point: Option<POINT>,
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

/// Access the single application instance, if it is still initialised.
///
/// SAFETY: only the main thread may call this while the App is alive. Window
/// procedures can still receive messages while the application shuts down and
/// the pointer is cleared, so they must use this function and tolerate None
/// instead of dereferencing a stale pointer.
pub unsafe fn try_app_mut<'a>() -> Option<&'a mut App> {
    let pointer = APP.load(Ordering::Acquire);
    if pointer.is_null() {
        None
    } else {
        Some(&mut *pointer)
    }
}

/// Create the mutex used for single instance detection. Returns the handle and
/// whether another instance already owns the mutex.
fn acquire_instance_mutex() -> (HANDLE, bool) {
    // Debug UI tests may opt into an isolated instance so they never stop or
    // impersonate the user's running release. Release builds always use the
    // fixed production mutex name.
    #[cfg(debug_assertions)]
    let mutex_name = std::env::var("VIBEKEY_DEBUG_INSTANCE")
        .ok()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 40
                && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .map(|value| format!("Local\\Vibekey.SingleInstance.Debug.{value}"))
        .unwrap_or_else(|| "Local\\Vibekey.SingleInstance".to_string());
    #[cfg(not(debug_assertions))]
    let mutex_name = "Local\\Vibekey.SingleInstance".to_string();
    let name = wide(&mutex_name);
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

    // Every access below is a short lived reference: no `&mut App` is kept
    // across the message loop or across calls that pump Windows messages.
    // SAFETY: the pointer was just created, is owned only by this function and
    // only the main thread touches it.
    let created = unsafe { (*pointer).create_window() };
    if !created {
        log::error!("could not create the main window");
        APP.store(std::ptr::null_mut(), Ordering::Release);
        // SAFETY: the box was created above and is released exactly once.
        unsafe {
            drop(Box::from_raw(pointer));
        }
        return 1;
    }
    // SAFETY: see above; no reference outlives the call.
    let startup_warning = unsafe { (*pointer).start() };
    if let Some(text) = startup_warning {
        // Show the error outside of start(), so that message pumping cannot
        // re-enter a window procedure while a &mut App is borrowed.
        // SAFETY: short lived read of the window handle.
        let hwnd = unsafe { (*pointer).hwnd };
        util::message_box_warning(hwnd, &text, "Vibekey");
    }
    let exit_code = message_loop(pointer);

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
            history: ClipboardHistory::new(),
            history_control: Arc::new(HistoryControl::new()),
            clipboard_listener: false,
            settings: std::ptr::null_mut(),
            last_mtime: None,
            last_notify: HashMap::new(),
            sync_in_progress: false,
            taskbar_created: 0,
            pending_menu_point: None,
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
    ///
    /// Returns a message that has to be shown by the caller *after* this method
    /// returned: showing it here would pump messages while `&mut self` is
    /// borrowed, which the aliasing contract in the module documentation forbids.
    fn start(&mut self) -> Option<String> {
        // SAFETY: the message string is a valid NUL terminated buffer.
        self.taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };

        let warning = self.load_initial_config();
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

        self.inject = Some(inject::start(self.hwnd, Arc::clone(&self.history_control)));

        // AddClipboardFormatListener is available on Windows 7. Notifications
        // only read CF_UNICODETEXT; rich payloads and files are never cloned.
        // SAFETY: the main window is live and belongs to this message-loop thread.
        self.clipboard_listener = unsafe { AddClipboardFormatListener(self.hwnd) } != 0;
        if self.clipboard_listener {
            self.capture_clipboard();
        } else {
            self.notify(
                "history-listener",
                "无法监听剪贴板变化，历史记录暂不可用",
                true,
            );
        }

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
        warning
    }

    /// Load the configuration, falling back to defaults when the file is broken
    /// (the broken file is never overwritten). The returned text has to be shown
    /// as an error dialog by the caller, outside of this method.
    fn load_initial_config(&mut self) -> Option<String> {
        let warning = match Config::load_or_create(&self.config_path) {
            Ok(config) => {
                self.config = config;
                None
            }
            Err(error) => {
                log::error!("could not load the configuration: {error}");
                self.config = Config::default();
                Some(format!(
                    "读取配置失败，已使用默认配置（不会覆盖原文件）。\n\n{}\n\n{error}",
                    self.config_path.display()
                ))
            }
        };
        self.rebuild_keymap(false);
        self.last_mtime = file_mtime(&self.config_path);
        warning
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

    /// Capture one clipboard-update notification without sleeping on the UI
    /// thread. Internal paste/restore updates are filtered by HistoryControl.
    fn capture_clipboard(&mut self) {
        if let Some(text) = self.history_control.capture_text() {
            if self.history.record(text) {
                log::debug!(
                    "clipboard history now has {} entries",
                    self.history.entries().count()
                );
            }
        } else {
            log::debug!("clipboard update was internal, locked, or contained no Unicode text");
        }
    }

    #[cfg(debug_assertions)]
    fn debug_seed_history(&mut self) {
        if std::env::var_os("VIBEKEY_DEBUG_INSTANCE").is_none() {
            return;
        }
        self.history.clear();
        self.history.record("历史甲 第一行".to_string());
        self.history.record("历史乙\r\n第二行 {date}".to_string());
    }

    /// Queue a snippet for injection.
    ///
    /// The template is expanded on the inject worker thread, so the clipboard is
    /// only read by the thread that also owns the clipboard restore.
    pub fn inject_snippet(&mut self, index: usize, target: HWND) {
        let Some(snippet) = self.config.snippet.get(index) else {
            return;
        };
        if !snippet.enabled {
            return;
        }
        let template = snippet.text.clone();
        let send_enter = snippet.send_enter;
        let target = if target.is_null() {
            // SAFETY: GetForegroundWindow has no preconditions.
            unsafe { GetForegroundWindow() }
        } else {
            target
        };
        let job = InjectJob {
            template,
            expand_template: true,
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

    /// Paste one history entry verbatim. The stable ID is revalidated because a
    /// newer copy or Delete may have changed the list while the popup was open.
    pub fn inject_history(&mut self, id: u64, target: HWND) {
        let Some(text) = self.history.text(id).map(str::to_owned) else {
            self.notify("history-stale", "这条剪贴板历史已被删除", true);
            return;
        };
        let target = if target.is_null() {
            // SAFETY: GetForegroundWindow has no preconditions.
            unsafe { GetForegroundWindow() }
        } else {
            target
        };
        let job = InjectJob {
            template: text,
            expand_template: false,
            send_enter: false,
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
            self.notify("inject", "无法启动输入线程，粘贴历史失败", true);
        }
    }

    /// Open (or close) the snippet picker.
    pub fn open_list(&mut self) {
        self.open_list_with_hint(std::ptr::null_mut());
    }

    /// Open the picker, preferring the window that triggered the hotkey.
    fn open_list_with_hint(&mut self, hint: HWND) {
        if self.popup.is_open() {
            self.popup.hide();
            return;
        }
        let items: Vec<PopupItem> = self
            .config
            .snippet
            .iter()
            .enumerate()
            .filter(|(_, snippet)| snippet.enabled)
            .map(|(index, snippet)| PopupItem::from_snippet(index, snippet))
            .collect();
        if items.is_empty() {
            self.notify_always("还没有可用片段，请先创建一个", false);
            self.open_snippet_editor(None);
            return;
        }
        let target = self.picker_target(hint);
        if !self
            .popup
            .show(self.hwnd, PickerMode::Snippets, items, target)
        {
            self.notify("popup", "无法打开片段列表", true);
        }
    }

    /// Open (or close) the memory-only text clipboard history.
    pub fn open_history(&mut self) {
        self.open_history_with_hint(std::ptr::null_mut());
    }

    fn open_history_with_hint(&mut self, hint: HWND) {
        if self.popup.is_open() {
            self.popup.hide();
            return;
        }
        let items = self
            .history
            .entries()
            .map(|entry| PopupItem::from_history(entry.id(), entry.text()))
            .collect();
        let target = self.picker_target(hint);
        if !self
            .popup
            .show(self.hwnd, PickerMode::History, items, target)
        {
            self.notify("popup", "无法打开剪贴板历史", true);
        }
    }

    fn picker_target(&self, hint: HWND) -> HWND {
        // SAFETY: IsWindow accepts any handle and only reads.
        if !hint.is_null()
            && unsafe { IsWindow(hint) } != 0
            && !util::is_our_process_window(hint)
            && !util::is_shell_surface(hint)
        {
            return hint;
        }
        self.injection_target()
    }

    /// Window that should receive the injection when the list is opened.
    ///
    /// The last window that really received a key press wins: when the picker
    /// is opened from the tray icon the foreground window is the shell
    /// (Shell_TrayWnd/Progman), which is never a valid injection target.
    fn injection_target(&self) -> HWND {
        if let Some(handle) = self.hook.as_ref() {
            let candidate = handle.shared().last_foreground();
            // SAFETY: IsWindow accepts any handle and only reads.
            if !candidate.is_null()
                && unsafe { IsWindow(candidate) } != 0
                && !util::is_our_process_window(candidate)
                && !util::is_shell_surface(candidate)
            {
                return candidate;
            }
        }
        usable_foreground_window()
    }

    /// Queue a native menu instead of entering TrackPopupMenu while the caller
    /// holds an App borrow. show_queued_menu reacquires state before/after the
    /// pumping call, keeping nested window messages alias-safe.
    pub(crate) fn queue_menu(&mut self, point: POINT) {
        self.pending_menu_point = Some(point);
        // SAFETY: the main window is live; posting never pumps nested messages.
        unsafe {
            PostMessageW(self.hwnd, WM_APP_SHOW_MENU, 0, 0);
        }
    }

    fn handle_menu_command(&mut self, command: u32) {
        match command {
            menu::CMD_LIST => self.open_list(),
            menu::CMD_HISTORY => self.open_history(),
            menu::CMD_CLEAR_HISTORY => {
                self.history.clear();
                self.notify_always("剪贴板历史已清空", false);
            }
            menu::CMD_NEW_SNIPPET => self.open_snippet_editor(None),
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

    /// Open the snippet page directly at an existing item, or create a new one.
    /// This is used by F2/right-click in the picker and by the tray menu.
    pub fn open_snippet_editor(&mut self, index: Option<usize>) {
        self.popup.hide();
        // The settings window owns a working copy which may have unsaved
        // insertions/deletions. Carry the selected value as well as its index
        // so the editor can resolve identity instead of trusting two snapshots
        // to have identical positions.
        let expected = index
            .and_then(|value| self.config.snippet.get(value))
            .cloned();
        if index.is_some() && expected.is_none() {
            self.notify("editor", "该片段已不存在，请刷新列表后重试", true);
            return;
        }
        // SAFETY: IsWindow accepts any handle.
        if !self.settings.is_null() && unsafe { IsWindow(self.settings) } != 0 {
            settings::show_snippet_editor(self.settings, index, expected.as_ref());
            // SAFETY: the settings handle was just verified live.
            unsafe {
                SetForegroundWindow(self.settings);
            }
            return;
        }
        self.open_settings();
        // SAFETY: open_settings either stores a live handle or null.
        if !self.settings.is_null() && unsafe { IsWindow(self.settings) } != 0 {
            settings::show_snippet_editor(self.settings, index, expected.as_ref());
        }
    }

    fn show_about(&self) {
        let hotkeys = match self.keymap.as_ref() {
            Some(keymap) => format!(
                "片段列表：{}\n剪贴板历史：{}\n配置菜单：{}",
                keymap.list_hotkey().display(),
                keymap.history_hotkey().display(),
                keymap.config_hotkey().display()
            ),
            None => format!(
                "片段列表：{}\n剪贴板历史：{}\n配置菜单：{}",
                self.config.hotkeys.list, self.config.hotkeys.history, self.config.hotkeys.config
            ),
        };
        let text = format!(
            "Vibekey {}\n\n{}\n\n配置文件：{}\n\n按住 Alt 按数字键即可插入片段；Alt+V 打开文本剪贴板历史。\n支持 Windows 7 SP1 及更新版本。",
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
        // The hook queues (action, foreground window) pairs in message order, so
        // the window matches this action even when several actions are pending.
        let trigger = self
            .hook
            .as_ref()
            .map(|handle| handle.shared().take_trigger(action))
            .unwrap_or(std::ptr::null_mut());
        match action {
            ACTION_LIST => self.open_list_with_hint(trigger),
            ACTION_HISTORY => self.open_history_with_hint(trigger),
            ACTION_CONFIG => {
                let point = util::caret_pos();
                self.queue_menu(point);
            }
            ACTION_SNIPPET => {
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
                self.queue_menu(point);
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
            KillTimer(self.hwnd, TIMER_TRAY_CLICK);
        }
        if self.clipboard_listener {
            // SAFETY: this thread registered the listener for the still-live main
            // window; removal prevents any further history messages during exit.
            unsafe {
                RemoveClipboardFormatListener(self.hwnd);
            }
            self.clipboard_listener = false;
        }
        self.history.clear();
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

/// Run the main message loop until WM_QUIT.
///
/// Takes the raw App pointer instead of a reference so that no `&mut App` is
/// alive while GetMessageW blocks and a window procedure may access the App.
fn message_loop(pointer: *mut App) -> i32 {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: message is a valid out parameter; a null window filter
        // retrieves messages for the whole thread.
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result == 0 || result == -1 {
            break;
        }
        // SAFETY: short lived reads of window handles; only the main thread
        // touches App and no reference is kept across the dispatch below.
        let (main_hwnd, settings) = unsafe { ((*pointer).hwnd, (*pointer).settings) };
        let mut handled = false;
        // IsDialogMessageW is only for messages belonging to the settings
        // dialog. Clipboard updates, tray callbacks and worker completions are
        // addressed to the hidden main window and must never be consumed as
        // dialog navigation while settings happens to be open.
        if message.hwnd != main_hwnd && !settings.is_null() && unsafe { IsWindow(settings) } != 0 {
            // SAFETY: the message is valid and the window is live; the call only
            // reads the message structure. It may dispatch WM_COMMAND to the
            // settings window but never touches App directly.
            handled = unsafe { IsDialogMessageW(settings, &message) } != 0;
        }
        if !handled {
            // SAFETY: the message was filled in by GetMessageW; window
            // procedures re-read the App state from the APP pointer.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    message.wParam as i32
}

/// Show a queued menu without holding an App reference across TrackPopupMenu.
/// State is copied in one short borrow; command dispatch uses a fresh borrow.
fn show_queued_menu() {
    // SAFETY: called by the main window procedure on the App's main thread.
    let request = unsafe { try_app_mut() }.and_then(|app| {
        app.pending_menu_point.take().map(|point| {
            (
                app.hwnd,
                point,
                MenuState {
                    sync_available: app.config.sync.enabled,
                    syncing: app.sync_in_progress,
                    autostart: app.config.start_with_windows,
                    history_available: !app.history.is_empty(),
                },
            )
        })
    });
    let Some((hwnd, point, state)) = request else {
        return;
    };
    if let Some(command) = menu::show(hwnd, point, &state) {
        // SAFETY: the modal menu has returned; nested messages are complete and
        // the App may be reborrowed for the selected command.
        if let Some(app) = unsafe { try_app_mut() } {
            app.handle_menu_command(command);
        }
    }
}

unsafe extern "system" fn main_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Handle this before the procedure's general App borrow. menu::show pumps
    // messages, so no outer &mut App may remain alive during the call.
    if message == WM_APP_SHOW_MENU {
        show_queued_menu();
        return 0;
    }

    // SAFETY: window procedures only run on the main thread; during shutdown the
    // app may already be gone, in which case messages fall through to the
    // default handling.
    let Some(app) = try_app_mut() else {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    };

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
        WM_CLIPBOARDUPDATE => {
            app.capture_clipboard();
            0
        }
        #[cfg(debug_assertions)]
        WM_APP_DEBUG_HISTORY => {
            app.debug_seed_history();
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

/// Foreground window when it belongs to another process and is not a shell
/// surface, otherwise null.
fn usable_foreground_window() -> HWND {
    // SAFETY: GetForegroundWindow has no preconditions.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return std::ptr::null_mut();
    }
    if util::is_our_process_window(foreground) || util::is_shell_surface(foreground) {
        std::ptr::null_mut()
    } else {
        foreground
    }
}
