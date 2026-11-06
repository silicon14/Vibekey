//! Input injection worker thread.
//!
//! The main thread only posts a job; the worker waits for the physical Alt key
//! to be released, restores the target window and then either pastes through the
//! clipboard (default, best for CJK text) or types the text with
//! SendInput(KEYEVENTF_UNICODE). The clipboard text saved before the paste is
//! handed back to the main thread, which restores it from a timer callback so
//! that the worker never owns the clipboard for long.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use vibekey_core::InputMethod;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HWND};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VK_CONTROL, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsWindow, PostMessageW, SetForegroundWindow,
};

use crate::clipboard;
use crate::util::{WM_APP_CLIP_RESTORE, WM_APP_INJECT_DONE};

const ALT_RELEASE_TIMEOUT: Duration = Duration::from_millis(2000);
const MODIFIER_RELEASE_TIMEOUT: Duration = Duration::from_millis(100);
/// Delay between the paste and the clipboard restoration.
pub const CLIPBOARD_RESTORE_DELAY_MS: u32 = 250;

/// One injection request.
pub struct InjectJob {
    pub text: String,
    pub send_enter: bool,
    /// Target window as a raw handle value; stored as isize because HWND is not
    /// Send and the job crosses the thread boundary.
    pub target: isize,
    pub input_method: InputMethod,
    pub restore_clipboard: bool,
}

/// State shared between the inject thread and the main window.
pub struct InjectShared {
    main_hwnd: AtomicIsize,
    pending_restore: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
}

impl InjectShared {
    /// Clipboard text the main thread still has to restore, if any.
    pub fn take_restore(&self) -> Option<String> {
        self.pending_restore
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Last injection error message, if any.
    pub fn take_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|mut slot| slot.take())
    }

    fn set_error(&self, message: impl Into<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(message.into());
        }
    }
}

/// Handle to the inject thread.
pub struct InjectHandle {
    sender: Option<mpsc::Sender<InjectJob>>,
    shared: Arc<InjectShared>,
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl InjectHandle {
    pub fn shared(&self) -> &Arc<InjectShared> {
        &self.shared
    }

    /// Queue a job. Returns false when the worker is gone.
    pub fn submit(&self, job: InjectJob) -> bool {
        let Some(sender) = self.sender.as_ref() else {
            return false;
        };
        match sender.send(job) {
            Ok(()) => true,
            Err(error) => {
                log::error!("could not queue an injection job: {error}");
                false
            }
        }
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.sender = None;
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for InjectHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the injection worker.
pub fn start(main_hwnd: HWND) -> InjectHandle {
    let shared = Arc::new(InjectShared {
        main_hwnd: AtomicIsize::new(crate::util::hwnd_to_isize(main_hwnd)),
        pending_restore: Mutex::new(None),
        last_error: Mutex::new(None),
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel::<InjectJob>();
    let worker_shared = Arc::clone(&shared);
    let worker_shutdown = Arc::clone(&shutdown);
    let join = std::thread::Builder::new()
        .name("vibekey-inject".to_string())
        .spawn(move || worker(worker_shared, worker_shutdown, receiver))
        .ok();
    InjectHandle {
        sender: Some(sender),
        shared,
        shutdown,
        join,
    }
}

fn worker(
    shared: Arc<InjectShared>,
    shutdown: Arc<AtomicBool>,
    receiver: mpsc::Receiver<InjectJob>,
) {
    loop {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(job) => run_job(&shared, &shutdown, job),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn run_job(shared: &InjectShared, shutdown: &AtomicBool, job: InjectJob) {
    let result = deliver(shared, shutdown, &job);
    match &result {
        Ok(()) => {}
        Err(error) => {
            log::warn!("injection failed: {error}");
            shared.set_error(error.clone());
        }
    }
    let code = if result.is_ok() { 0 } else { 1 };
    post(shared, WM_APP_INJECT_DONE, code, 0);
}

fn post(shared: &InjectShared, message: u32, wparam: usize, lparam: isize) {
    let hwnd = crate::util::isize_to_hwnd(shared.main_hwnd.load(Ordering::Acquire));
    if hwnd.is_null() {
        return;
    }
    // SAFETY: the window handle belongs to the main thread and the message is
    // one of our private WM_APP messages.
    unsafe {
        PostMessageW(hwnd, message, wparam, lparam);
    }
}

fn deliver(shared: &InjectShared, shutdown: &AtomicBool, job: &InjectJob) -> Result<(), String> {
    if job.text.is_empty() && !job.send_enter {
        return Err("片段内容为空".to_string());
    }
    wait_alt_release(shutdown, ALT_RELEASE_TIMEOUT);
    if shutdown.load(Ordering::Acquire) {
        return Err("正在退出".to_string());
    }

    let mut target = crate::util::isize_to_hwnd(job.target);
    // SAFETY: IsWindow accepts any handle and only reads.
    if !target.is_null() && unsafe { IsWindow(target) } == 0 {
        target = std::ptr::null_mut();
    }
    if !target.is_null() {
        // SAFETY: the handle is a live window.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground != target {
            // SAFETY: the handle is a live window; the call is allowed to fail
            // when the foreground window cannot be changed.
            let changed = unsafe { SetForegroundWindow(target) } != 0;
            if !changed {
                log::warn!("SetForegroundWindow failed; injecting into the current focus");
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        if is_elevated(target) {
            return Err("无法向管理员权限窗口输入：请以管理员身份运行 Vibekey".to_string());
        }
    }

    // A physical Ctrl/Shift/Alt/Win left over from the hotkey would corrupt the
    // pasted text, so wait briefly for the user to let go.
    wait_modifiers_release(shutdown, MODIFIER_RELEASE_TIMEOUT);

    match job.input_method {
        InputMethod::Paste => match paste(shared, job) {
            Ok(()) => Ok(()),
            Err(error) => {
                log::warn!("clipboard paste failed ({error}); falling back to typing");
                type_text(shutdown, job)
            }
        },
        InputMethod::Type => type_text(shutdown, job),
    }
}

/// True when the process that owns the window cannot be opened for query
/// because of its integrity level (UIPI).
fn is_elevated(hwnd: HWND) -> bool {
    let mut process_id = 0u32;
    // SAFETY: process_id is a valid out pointer.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    if process_id == 0 {
        return false;
    }
    // SAFETY: the process handle is closed below in every path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if handle.is_null() {
            return GetLastError() == 5;
        }
        CloseHandle(handle);
    }
    false
}

fn key_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions.
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

fn any_modifier_down() -> bool {
    key_down(VK_MENU)
        || key_down(VK_CONTROL)
        || key_down(VK_SHIFT)
        || key_down(VK_LWIN)
        || key_down(VK_RWIN)
}

fn wait_alt_release(shutdown: &AtomicBool, timeout: Duration) {
    let start = Instant::now();
    while key_down(VK_MENU) && start.elapsed() < timeout {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if key_down(VK_MENU) {
        log::warn!("Alt is still held after the timeout; injecting anyway");
    }
}

fn wait_modifiers_release(shutdown: &AtomicBool, timeout: Duration) {
    let start = Instant::now();
    while any_modifier_down() && start.elapsed() < timeout {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if any_modifier_down() {
        log::warn!("modifier key still held; sending the input anyway");
    }
}

fn key_event(vk: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_event(unit: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> bool {
    if inputs.is_empty() {
        return true;
    }
    // SAFETY: the slice pointer and element count match and the INPUT structure
    // size is passed as required by the API.
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    sent == inputs.len() as u32
}

fn tap(vk: u16) -> bool {
    send(&[key_event(vk, 0), key_event(vk, KEYEVENTF_KEYUP)])
}

fn press_control_v() -> bool {
    let events = [
        key_event(VK_CONTROL, 0),
        key_event(0x56, 0),
        key_event(0x56, KEYEVENTF_KEYUP),
        key_event(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    send(&events)
}

/// Clipboard based delivery: copy, Ctrl+V, optional Enter, then hand the old
/// clipboard text back to the main thread.
fn paste(shared: &InjectShared, job: &InjectJob) -> Result<(), String> {
    let saved = clipboard::read_text();
    if !clipboard::write_text(&job.text) {
        return Err("无法写入剪贴板".to_string());
    }
    std::thread::sleep(Duration::from_millis(50));
    if !press_control_v() {
        return Err("SendInput 发送 Ctrl+V 失败".to_string());
    }
    if job.send_enter {
        std::thread::sleep(Duration::from_millis(30));
        if !tap(VK_RETURN) {
            return Err("SendInput 发送回车失败".to_string());
        }
    }
    if job.restore_clipboard {
        if let Some(saved) = saved {
            if let Ok(mut slot) = shared.pending_restore.lock() {
                *slot = Some(saved);
            }
            // The main thread sleeps CLIPBOARD_RESTORE_DELAY and then restores
            // the clipboard, so the paste in the target application is done.
            post(shared, WM_APP_CLIP_RESTORE, 0, 0);
        }
    }
    Ok(())
}

/// SendInput(KEYEVENTF_UNICODE) based delivery.
fn type_text(shutdown: &AtomicBool, job: &InjectJob) -> Result<(), String> {
    let mut count = 0usize;
    for character in job.text.chars() {
        if shutdown.load(Ordering::Acquire) {
            return Err("正在退出".to_string());
        }
        if character == '\r' {
            continue;
        }
        if character == '\n' {
            if !tap(VK_RETURN) {
                return Err("SendInput 发送回车失败".to_string());
            }
        } else {
            let mut buffer = [0u16; 2];
            for unit in character.encode_utf16(&mut buffer) {
                if !send(&[
                    unicode_event(*unit, 0),
                    unicode_event(*unit, KEYEVENTF_KEYUP),
                ]) {
                    return Err("SendInput 输入字符失败".to_string());
                }
            }
        }
        count += 1;
        std::thread::sleep(Duration::from_millis(2));
        if count % 200 == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if job.send_enter {
        std::thread::sleep(Duration::from_millis(30));
        if !tap(VK_RETURN) {
            return Err("SendInput 发送回车失败".to_string());
        }
    }
    Ok(())
}
