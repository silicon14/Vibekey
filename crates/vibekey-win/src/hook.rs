//! Global low level keyboard hook (WH_KEYBOARD_LL).
//!
//! The hook runs on its own thread with a message loop. The callback only does
//! cheap work: it reads the shared keymap, decides whether a hotkey matched and
//! posts a message to the main window. It never performs file or network IO,
//! which would make Windows remove the hook after the low level hook timeout.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};

use vibekey_core::{ActivationMode, Binding, Hotkey, Key, Keymap, Modifiers};
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24,
    VK_HOME, VK_INSERT, VK_LEFT, VK_LMENU, VK_LWIN, VK_MENU, VK_NEXT, VK_OEM_3, VK_OEM_MINUS,
    VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    PeekMessageW, PostMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
};

use crate::util::{self, isize_to_hwnd, ACTION_CONFIG, ACTION_LIST, ACTION_SNIPPET, WM_APP_ACTION};

/// Configuration snapshot shared between the main thread and the hook thread.
#[derive(Clone)]
pub struct HookConfig {
    pub keymap: Option<Keymap>,
    pub activation: ActivationMode,
    pub hold_ms: u32,
    pub ignore_apps: Vec<String>,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            keymap: None,
            activation: ActivationMode::Immediate,
            hold_ms: 350,
            ignore_apps: Vec::new(),
        }
    }
}

impl HookConfig {
    /// Build the snapshot from a loaded configuration and its keymap.
    pub fn from_config(config: &vibekey_core::Config, keymap: Option<Keymap>) -> Self {
        let ignore_apps = config
            .ignore_apps
            .iter()
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect();
        Self {
            keymap,
            activation: config.activation,
            hold_ms: config.hold_ms,
            ignore_apps,
        }
    }
}

/// State shared with the hook thread.
pub struct HookShared {
    config: RwLock<HookConfig>,
    main_hwnd: AtomicIsize,
    trigger_hwnd: AtomicIsize,
    our_process: u32,
    /// Last queried process name; avoids an OpenProcess call on every hotkey.
    process_cache: std::sync::Mutex<Option<(u32, Option<String>)>>,
    /// Foreground window of the last key press that went to another process.
    /// Used as the injection target when the snippet list is opened from the
    /// tray icon, where the foreground window is the shell itself.
    last_foreground: AtomicIsize,
}

impl HookShared {
    fn new() -> Self {
        Self {
            config: RwLock::new(HookConfig::default()),
            main_hwnd: AtomicIsize::new(0),
            trigger_hwnd: AtomicIsize::new(0),
            our_process: util::current_process_id(),
            process_cache: std::sync::Mutex::new(None),
            last_foreground: AtomicIsize::new(0),
        }
    }

    /// Foreground window of the last key press that went to another process.
    pub fn last_foreground(&self) -> HWND {
        isize_to_hwnd(self.last_foreground.load(Ordering::Relaxed))
    }

    /// File name of the process that owns the window, cached for the last query.
    /// A try_lock keeps the hook callback from ever blocking.
    fn process_name(&self, process_id: u32) -> Option<String> {
        if let Ok(cache) = self.process_cache.try_lock() {
            if let Some((cached_id, name)) = cache.as_ref() {
                if *cached_id == process_id {
                    return name.clone();
                }
            }
        }
        let name = util::process_name(process_id);
        if let Ok(mut cache) = self.process_cache.try_lock() {
            *cache = Some((process_id, name.clone()));
        }
        name
    }

    pub fn set_config(&self, config: HookConfig) {
        if let Ok(mut slot) = self.config.write() {
            *slot = config;
        }
    }

    pub fn set_main_hwnd(&self, hwnd: HWND) {
        self.main_hwnd
            .store(util::hwnd_to_isize(hwnd), Ordering::Release);
    }

    /// Foreground window that triggered the last snippet action, or null.
    pub fn take_trigger_hwnd(&self) -> HWND {
        isize_to_hwnd(self.trigger_hwnd.swap(0, Ordering::AcqRel))
    }
}

/// The single installed hook. Its callback has no user data argument, so the
/// shared state is reached through this atomic pointer which is only set before
/// the hook is installed and cleared after it has been removed.
static SHARED: AtomicPtr<HookShared> = AtomicPtr::new(std::ptr::null_mut());
static ALT_DOWN_MS: AtomicU64 = AtomicU64::new(0);
/// Last virtual key that went down and whether its key up was seen yet. A held
/// key produces repeated WM_KEYDOWN messages without an intervening key up;
/// those auto repeats must not trigger the snippet again.
static LAST_VK: AtomicU32 = AtomicU32::new(0);
static LAST_VK_DOWN: AtomicBool = AtomicBool::new(false);

/// Milliseconds since the process started.
fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn is_physically_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions; the high bit of the result
    // tells whether the key is currently down.
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// VK to core Key mapping. Only the keys that can appear in a hotkey are mapped.
fn map_virtual_key(vk: u16) -> Option<Key> {
    let key = match vk {
        VK_BACK => Key::Backspace,
        VK_TAB => Key::Tab,
        VK_RETURN => Key::Enter,
        VK_ESCAPE => Key::Escape,
        VK_SPACE => Key::Space,
        VK_DELETE => Key::Delete,
        VK_INSERT => Key::Insert,
        VK_LEFT => Key::Left,
        VK_RIGHT => Key::Right,
        VK_UP => Key::Up,
        VK_DOWN => Key::Down,
        VK_HOME => Key::Home,
        VK_END => Key::End,
        VK_PRIOR => Key::PageUp,
        VK_NEXT => Key::PageDown,
        VK_OEM_3 => Key::Char(0x60 as char),
        VK_OEM_MINUS => Key::Char('-'),
        VK_OEM_PLUS => Key::Char('='),
        _ if (VK_F1..=VK_F24).contains(&vk) => Key::F((vk - VK_F1 + 1) as u8),
        _ if (0x41..=0x5A).contains(&vk) => Key::Char((vk as u8 as char).to_ascii_lowercase()),
        _ if (0x30..=0x39).contains(&vk) => Key::Char(vk as u8 as char),
        _ => return None,
    };
    Some(key)
}

/// The hook callback.
///
/// SAFETY: Windows calls this with a valid KBDLLHOOKSTRUCT pointer in lparam
/// when ncode >= 0. The shared pointer is only non-null while the hook is
/// installed.
unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let next = || CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    if code < 0 {
        return next();
    }
    let shared_pointer = SHARED.load(Ordering::Acquire);
    if shared_pointer.is_null() || lparam == 0 {
        return next();
    }
    let shared = &*shared_pointer;
    let keyboard = &*(lparam as *const KBDLLHOOKSTRUCT);

    // Our own injected input must never trigger a hotkey.
    if keyboard.flags & LLKHF_INJECTED != 0 {
        return next();
    }

    let message = wparam as u32;
    let is_down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let is_up = message == WM_KEYUP || message == WM_SYSKEYUP;

    let vk = keyboard.vkCode as u16;
    if vk == VK_MENU || vk == VK_LMENU || vk == VK_RMENU {
        if is_down {
            ALT_DOWN_MS.store(monotonic_ms().max(1), Ordering::Release);
        } else if is_up {
            ALT_DOWN_MS.store(0, Ordering::Release);
        }
        return next();
    }
    if !is_down {
        // Key up: the key may trigger again on its next press.
        if LAST_VK.load(Ordering::Relaxed) == vk as u32 {
            LAST_VK_DOWN.store(false, Ordering::Relaxed);
        }
        return next();
    }

    let now = monotonic_ms();
    // Auto repeat: Windows repeats WM_KEYDOWN for a held key. The guard is
    // therefore "the same key is still down", which also covers the 80 ms rule
    // from the design document.
    if LAST_VK.load(Ordering::Relaxed) == vk as u32 && LAST_VK_DOWN.load(Ordering::Relaxed) {
        return next();
    }
    LAST_VK.store(vk as u32, Ordering::Relaxed);
    LAST_VK_DOWN.store(true, Ordering::Relaxed);

    // Alt may have been pressed before this hook was installed.
    if is_physically_down(VK_MENU) && ALT_DOWN_MS.load(Ordering::Relaxed) == 0 {
        ALT_DOWN_MS.store(now.max(1), Ordering::Relaxed);
    }

    let foreground = GetForegroundWindow();
    if foreground.is_null() {
        return next();
    }
    let mut process_id = 0u32;
    GetWindowThreadProcessId(foreground, &mut process_id);
    if process_id == shared.our_process {
        return next();
    }
    // Cheap single store; used as the injection target when the picker is
    // opened from the tray icon.
    shared
        .last_foreground
        .store(foreground as isize, Ordering::Relaxed);

    // try_read keeps the callback from ever blocking behind the reload on the
    // main thread.
    let Ok(config) = shared.config.try_read() else {
        return next();
    };
    let Some(keymap) = config.keymap.as_ref() else {
        return next();
    };
    let Some(key) = map_virtual_key(vk) else {
        return next();
    };

    let modifiers = Modifiers {
        alt: is_physically_down(VK_MENU),
        ctrl: is_physically_down(VK_CONTROL),
        shift: is_physically_down(VK_SHIFT),
        win: is_physically_down(VK_LWIN) || is_physically_down(VK_RWIN),
    };
    let hotkey = Hotkey {
        mods: modifiers,
        key,
    };
    let Some(binding) = keymap.resolve(&hotkey) else {
        return next();
    };

    // The expensive part (an OpenProcess call) only runs for keys that are
    // actually bound, so normal typing stays free of syscalls.
    if !config.ignore_apps.is_empty() {
        let mut foreground_process = 0u32;
        GetWindowThreadProcessId(foreground, &mut foreground_process);
        if let Some(name) = shared.process_name(foreground_process) {
            if config.ignore_apps.iter().any(|app| app == &name) {
                return next();
            }
        }
    }

    // Hold mode: snippet hotkeys only fire once Alt was held long enough.
    if config.activation == ActivationMode::Hold && modifiers.alt {
        if let Binding::Snippet(_) = binding {
            let pressed_at = ALT_DOWN_MS.load(Ordering::Relaxed);
            if pressed_at == 0 || now.saturating_sub(pressed_at) < config.hold_ms as u64 {
                return next();
            }
        }
    }

    let main_hwnd = isize_to_hwnd(shared.main_hwnd.load(Ordering::Acquire));
    if main_hwnd.is_null() {
        return next();
    }
    let action = match binding {
        Binding::List => ACTION_LIST,
        Binding::Config => ACTION_CONFIG,
        Binding::Snippet(index) => {
            shared
                .trigger_hwnd
                .store(foreground as isize, Ordering::Release);
            let index = index as isize;
            drop(config);
            PostMessageW(main_hwnd, WM_APP_ACTION, ACTION_SNIPPET, index);
            return 1;
        }
    };
    drop(config);
    PostMessageW(main_hwnd, WM_APP_ACTION, action, 0);
    1
}

fn hook_thread(_shared: Arc<HookShared>, ready: mpsc::Sender<Result<u32, String>>) {
    // SAFETY: this runs on the thread that will own the hook; the message queue
    // is created by PeekMessage before the thread id is published so that
    // PostThreadMessageW from the main thread cannot race with it.
    let thread_id = unsafe { GetCurrentThreadId() };
    // SAFETY: a zeroed MSG is a valid out parameter for PeekMessageW.
    unsafe {
        let mut message: MSG = std::mem::zeroed();
        PeekMessageW(
            &mut message,
            std::ptr::null_mut(),
            WM_USER,
            WM_USER,
            PM_NOREMOVE,
        );
    }
    // SAFETY: a null module name asks for the module handle of the executable.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: the callback is a valid HOOKPROC and a thread id of 0 installs a
    // global hook.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) };
    if hook.is_null() {
        // SAFETY: GetLastError has no preconditions.
        let code = unsafe { GetLastError() };
        let _ = ready.send(Err(format!("SetWindowsHookExW 失败（错误 {code}）")));
        return;
    }
    if ready.send(Ok(thread_id)).is_err() {
        // The main thread disappeared; remove the hook and stop.
        // SAFETY: the hook handle is valid.
        unsafe {
            UnhookWindowsHookEx(hook);
        }
        return;
    }

    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: message is a valid out parameter for GetMessageW.
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result == 0 || result == -1 {
            break;
        }
        // SAFETY: the message was filled in by GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // SAFETY: the hook was installed above and is removed exactly once.
    unsafe {
        UnhookWindowsHookEx(hook);
    }
}

/// Handle to the hook thread.
pub struct HookHandle {
    thread_id: u32,
    installed: bool,
    shared: Arc<HookShared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl HookHandle {
    pub fn shared(&self) -> &Arc<HookShared> {
        &self.shared
    }

    pub fn stop(&mut self) {
        if let Some(join) = self.join.take() {
            if self.installed && self.thread_id != 0 {
                // SAFETY: the thread id belongs to the hook thread, which has a
                // message queue; WM_QUIT makes its GetMessageW return 0.
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                        self.thread_id,
                        WM_QUIT,
                        0,
                        0,
                    );
                }
            }
            let _ = join.join();
            self.installed = false;
        }
        SHARED.store(std::ptr::null_mut(), Ordering::Release);
    }
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Install the hook and return its handle, or None when installation failed.
pub fn start(main_hwnd: HWND) -> Option<HookHandle> {
    let shared = Arc::new(HookShared::new());
    shared.set_main_hwnd(main_hwnd);
    let shared_pointer = Arc::as_ptr(&shared) as *mut HookShared;
    SHARED.store(shared_pointer, Ordering::Release);
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread_shared = Arc::clone(&shared);
    let join = match std::thread::Builder::new()
        .name("vibekey-hook".to_string())
        .spawn(move || hook_thread(thread_shared, ready_tx))
    {
        Ok(join) => join,
        Err(error) => {
            SHARED.store(std::ptr::null_mut(), Ordering::Release);
            log::error!("could not start the hook thread: {error}");
            return None;
        }
    };
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(thread_id)) => Some(HookHandle {
            thread_id,
            installed: true,
            shared,
            join: Some(join),
        }),
        Ok(Err(error)) => {
            log::error!("{error}");
            let _ = join.join();
            SHARED.store(std::ptr::null_mut(), Ordering::Release);
            None
        }
        Err(error) => {
            log::error!("the hook thread did not report back: {error}");
            // The thread is still owned by the join handle, so shut it down
            // through the handle rather than leaking it.
            let mut handle = HookHandle {
                thread_id: 0,
                installed: false,
                shared,
                join: Some(join),
            };
            handle.stop();
            None
        }
    }
}
