//! Global low level keyboard hook (WH_KEYBOARD_LL).
//!
//! The hook runs on its own thread with a message loop. The callback only does
//! cheap work: it reads the shared keymap, decides whether a hotkey matched and
//! posts a message to the main window. It never performs file or network IO,
//! which would make Windows remove the hook after the low level hook timeout.

use std::collections::VecDeque;
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
    PeekMessageW, PostMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
};

use crate::util::{
    self, isize_to_hwnd, ACTION_CONFIG, ACTION_HISTORY, ACTION_LIST, ACTION_SNIPPET, WM_APP_ACTION,
};

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
    /// (action, foreground window) pairs for the messages posted to the main
    /// window. A queue keeps consecutive triggers paired with the right window.
    triggers: std::sync::Mutex<VecDeque<Trigger>>,
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
            triggers: std::sync::Mutex::new(VecDeque::new()),
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

    /// Remember the foreground window that caused an action. Called from the
    /// hook callback, so only try_lock is used and the queue is length capped.
    fn push_trigger(&self, action: usize, hwnd: HWND) {
        if let Ok(mut queue) = self.triggers.try_lock() {
            if queue.len() < 16 {
                queue.push_back(Trigger {
                    action,
                    hwnd: hwnd as isize,
                });
            }
        }
    }

    /// Foreground window that caused the oldest pending action of this kind, or
    /// null when the entry is missing (the caller then falls back to the current
    /// foreground window).
    pub fn take_trigger(&self, action: usize) -> HWND {
        if let Ok(mut queue) = self.triggers.lock() {
            if let Some(position) = queue.iter().position(|trigger| trigger.action == action) {
                if let Some(trigger) = queue.remove(position) {
                    return isize_to_hwnd(trigger.hwnd);
                }
            }
        }
        std::ptr::null_mut()
    }
}

/// One pending (action, foreground window) pair.
struct Trigger {
    action: usize,
    hwnd: isize,
}

/// The single installed hook. Its callback has no user data argument, so the
/// shared state is reached through this atomic pointer which is only set before
/// the hook is installed and cleared after it has been removed.
static SHARED: AtomicPtr<HookShared> = AtomicPtr::new(std::ptr::null_mut());
/// Wall clock (monotonic ms) of the current Alt press, 0 when Alt is up.
static ALT_DOWN_MS: AtomicU64 = AtomicU64::new(0);
/// Whether Alt is currently down. A held Alt produces repeated WM_SYSKEYDOWN
/// messages; only the 0 -> 1 edge may start the hold timer, otherwise hold mode
/// would never see the required hold time.
static ALT_PRESSED: AtomicBool = AtomicBool::new(false);
/// Last virtual key that went down and whether its key up was seen yet. A held
/// key produces repeated WM_KEYDOWN messages without an intervening key up;
/// those auto repeats must not trigger the snippet again.
static LAST_VK: AtomicU32 = AtomicU32::new(0);
static LAST_VK_DOWN: AtomicBool = AtomicBool::new(false);
/// Virtual key whose first key down was swallowed by a hotkey. Its auto repeats
/// have to be swallowed as well, otherwise the target application receives
/// repeated Alt+<digit> system key messages.
static CONSUMED_VK: AtomicU32 = AtomicU32::new(0);

/// Debug builds can be told to accept synthetic input so that the smoke test
/// script can drive the hook with SendInput. Release builds compile this away
/// and always ignore injected input.
#[cfg(debug_assertions)]
fn accept_injected_input() -> bool {
    static ACCEPT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ACCEPT.get_or_init(|| {
        let enabled = std::env::var_os("VIBEKEY_TEST_INJECTED").is_some();
        if enabled {
            log::warn!(
                "VIBEKEY_TEST_INJECTED is set: synthetic input can trigger hotkeys (debug build only)"
            );
        }
        enabled
    })
}

/// Release builds never accept synthetic input.
#[cfg(not(debug_assertions))]
fn accept_injected_input() -> bool {
    false
}

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
    if keyboard.flags & LLKHF_INJECTED != 0 && !accept_injected_input() {
        return next();
    }

    let message = wparam as u32;
    let is_down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let is_up = message == WM_KEYUP || message == WM_SYSKEYUP;

    let vk = keyboard.vkCode as u16;
    if vk == VK_MENU || vk == VK_LMENU || vk == VK_RMENU {
        if is_down {
            // Only the first key down of a press starts the hold timer; the
            // auto repeats of a held Alt must not refresh it.
            if !ALT_PRESSED.swap(true, Ordering::AcqRel) {
                ALT_DOWN_MS.store(monotonic_ms().max(1), Ordering::Release);
            }
        } else if is_up {
            ALT_PRESSED.store(false, Ordering::Release);
            ALT_DOWN_MS.store(0, Ordering::Release);
        }
        return next();
    }
    let consumed = CONSUMED_VK.load(Ordering::Relaxed);
    if !is_down {
        // Key up: the key may trigger again on its next press. A key up also
        // ends the "swallowed auto repeats" state.
        if consumed == vk as u32 {
            CONSUMED_VK.store(0, Ordering::Relaxed);
        }
        if LAST_VK.load(Ordering::Relaxed) == vk as u32 {
            LAST_VK_DOWN.store(false, Ordering::Relaxed);
        }
        return next();
    }
    if consumed == vk as u32 {
        // Auto repeat of a key whose first press triggered a hotkey: swallow it
        // as well so that the target application never sees Alt+<digit>.
        return 1;
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

    // Alt may have been pressed before this hook was installed: treat the first
    // key we see while Alt is physically down as the start of the hold timer.
    if is_physically_down(VK_MENU) && !ALT_PRESSED.swap(true, Ordering::AcqRel) {
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
    // From here on the key is swallowed: remember it so that its auto repeats are
    // swallowed too.
    CONSUMED_VK.store(vk as u32, Ordering::Release);
    let action = match binding {
        Binding::List => ACTION_LIST,
        Binding::History => ACTION_HISTORY,
        Binding::Config => ACTION_CONFIG,
        Binding::Snippet(index) => {
            #[cfg(debug_assertions)]
            log::debug!("snippet hotkey matched index {index}, target {foreground:?}");
            shared.push_trigger(ACTION_SNIPPET, foreground);
            let index = index as isize;
            drop(config);
            PostMessageW(main_hwnd, WM_APP_ACTION, ACTION_SNIPPET, index);
            return 1;
        }
    };
    shared.push_trigger(action, foreground);
    drop(config);
    PostMessageW(main_hwnd, WM_APP_ACTION, action, 0);
    1
}

fn hook_thread(shared: Arc<HookShared>, events: mpsc::Sender<HookEvent>) {
    HOOK_THREAD_ALIVE.store(true, Ordering::Release);
    run_hook_thread(shared, events);
    HOOK_THREAD_ALIVE.store(false, Ordering::Release);
}

fn run_hook_thread(_shared: Arc<HookShared>, events: mpsc::Sender<HookEvent>) {
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
    // The thread id is published before the hook is installed so that the main
    // thread can always shut this thread down, even when its start handshake
    // timed out before the Ready message arrived.
    HOOK_THREAD_ID.store(thread_id, Ordering::Release);
    if events.send(HookEvent::Ready(thread_id)).is_err() {
        // The main thread disappeared before the hook was installed.
        return;
    }
    // SAFETY: a null module name asks for the module handle of the executable.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: the callback is a valid HOOKPROC and a thread id of 0 installs a
    // global hook.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) };
    if hook.is_null() {
        // SAFETY: GetLastError has no preconditions.
        let code = unsafe { GetLastError() };
        let _ = events.send(HookEvent::Installed(Err(format!(
            "SetWindowsHookExW 失败（错误 {code}）"
        ))));
        return;
    }
    if events.send(HookEvent::Installed(Ok(()))).is_err() {
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

/// Handshake message from the hook thread to the main thread.
enum HookEvent {
    /// The message queue exists; the payload is the id that WM_QUIT can be
    /// posted to.
    Ready(u32),
    /// Result of SetWindowsHookExW.
    Installed(Result<(), String>),
}

/// Thread id of the hook thread, published as soon as its message queue exists.
/// It lets the main thread shut the thread down even when the start handshake
/// timed out before the Ready message was received.
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// True while the hook thread runs. It is set before the thread id is published
/// and cleared when the thread is about to exit, so that stop() never posts a
/// WM_QUIT to a thread id that may already have been reused.
static HOOK_THREAD_ALIVE: AtomicBool = AtomicBool::new(false);

/// Handle to the hook thread.
pub struct HookHandle {
    thread_id: u32,
    shared: Arc<HookShared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl HookHandle {
    pub fn shared(&self) -> &Arc<HookShared> {
        &self.shared
    }

    /// Stop the hook thread. A WM_QUIT is always posted before joining, so the
    /// thread can never be joined while it is blocked in GetMessageW.
    pub fn stop(&mut self) {
        if let Some(join) = self.join.take() {
            let mut thread_id = self.thread_id;
            if thread_id == 0 {
                thread_id = HOOK_THREAD_ID.load(Ordering::Acquire);
            }
            if thread_id != 0 && HOOK_THREAD_ALIVE.load(Ordering::Acquire) {
                // SAFETY: the id belongs to the hook thread, whose message queue
                // exists before the id is published; WM_QUIT makes its
                // GetMessageW return 0 and the thread removes the hook itself.
                unsafe {
                    PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
                }
                let _ = join.join();
            } else if thread_id != 0 {
                // The thread already exited on its own; joining cannot block.
                let _ = join.join();
            } else {
                // Without a thread id WM_QUIT cannot be delivered; detaching is
                // better than blocking the process exit. The thread notices the
                // closed handshake channel and exits on its own.
                log::warn!("hook thread id is unknown; detaching instead of joining");
                drop(join);
            }
        }
        HOOK_THREAD_ID.store(0, Ordering::Release);
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
    HOOK_THREAD_ID.store(0, Ordering::Release);
    HOOK_THREAD_ALIVE.store(false, Ordering::Release);
    let (event_tx, event_rx) = mpsc::channel();
    let thread_shared = Arc::clone(&shared);
    let join = match std::thread::Builder::new()
        .name("vibekey-hook".to_string())
        .spawn(move || hook_thread(thread_shared, event_tx))
    {
        Ok(join) => join,
        Err(error) => {
            SHARED.store(std::ptr::null_mut(), Ordering::Release);
            log::error!("could not start the hook thread: {error}");
            return None;
        }
    };

    // Two phase handshake: the thread id first, then the installation result.
    let mut thread_id = 0u32;
    let mut failure: Option<String> = None;
    match event_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(HookEvent::Ready(id)) => {
            thread_id = id;
            match event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(HookEvent::Installed(Ok(()))) => {}
                Ok(HookEvent::Installed(Err(error))) => failure = Some(error),
                Ok(HookEvent::Ready(_)) => {
                    failure =
                        Some("the hook thread sent an unexpected handshake message".to_string())
                }
                Err(error) => {
                    failure = Some(format!(
                        "the hook thread did not report the install result: {error}"
                    ))
                }
            }
        }
        Ok(HookEvent::Installed(_)) => {
            failure = Some("the hook thread sent an unexpected handshake message".to_string())
        }
        Err(error) => {
            thread_id = HOOK_THREAD_ID.load(Ordering::Acquire);
            failure = Some(format!(
                "the hook thread did not report its thread id: {error}"
            ));
        }
    }

    if let Some(error) = failure {
        log::error!("{error}");
        // Never join a thread that may still be waiting in GetMessageW: stop()
        // posts WM_QUIT first whenever the thread id is known.
        let mut handle = HookHandle {
            thread_id,
            shared,
            join: Some(join),
        };
        handle.stop();
        return None;
    }
    Some(HookHandle {
        thread_id,
        shared,
        join: Some(join),
    })
}
