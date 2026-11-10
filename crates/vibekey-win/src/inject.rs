//! Input injection worker thread.
//!
//! The main thread only posts a job; the worker waits for the physical Alt key
//! to be released, restores the target window and then either pastes through the
//! clipboard (default, best for CJK text) or types the text with
//! SendInput(KEYEVENTF_UNICODE).
//!
//! Clipboard safety: before a paste the worker inspects the clipboard formats.
//! When the clipboard holds anything but plain text the user asked to preserve
//! it (restore_clipboard), so the worker types the snippet instead and never
//! touches the clipboard; otherwise the plain text is saved, the snippet is
//! pasted and the old text is restored by the same worker after 250 ms. The
//! serial restore in the worker means a second snippet always starts from the
//! original clipboard, and no clipboard ownership is shared across threads.
//!
//! Template expansion (`{clipboard}`, `{date}`, ...) also happens in the worker,
//! right before the injection: the clipboard read for `{clipboard}` therefore
//! sees the original clipboard text and not the text of the previous snippet.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use vibekey_core::{expand, ExpandContext, InputMethod};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RETURN,
    VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, IsWindow, PostMessageW,
    SetForegroundWindow, GUITHREADINFO, GUI_INMENUMODE, GUI_POPUPMENUMODE,
};

use crate::clipboard;
use crate::history::HistoryControl;
use crate::util::WM_APP_INJECT_DONE;

const ALT_RELEASE_TIMEOUT: Duration = Duration::from_millis(2000);
const MODIFIER_RELEASE_TIMEOUT: Duration = Duration::from_millis(100);
/// Delay between the Ctrl+V and the clipboard restoration, so that slow target
/// applications have read the clipboard before it is rewritten.
const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(250);

/// One injection request.
pub struct InjectJob {
    /// Snippet template, or literal text for a clipboard-history entry.
    pub template: String,
    /// Snippets expand variables; clipboard history must always paste verbatim.
    pub expand_template: bool,
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
    last_error: Mutex<Option<String>>,
}

impl InjectShared {
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
pub fn start(main_hwnd: HWND, history_control: Arc<HistoryControl>) -> InjectHandle {
    let shared = Arc::new(InjectShared {
        main_hwnd: AtomicIsize::new(crate::util::hwnd_to_isize(main_hwnd)),
        last_error: Mutex::new(None),
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel::<InjectJob>();
    let worker_shared = Arc::clone(&shared);
    let worker_shutdown = Arc::clone(&shutdown);
    let join = std::thread::Builder::new()
        .name("vibekey-inject".to_string())
        .spawn(move || worker(worker_shared, worker_shutdown, history_control, receiver))
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
    history_control: Arc<HistoryControl>,
    receiver: mpsc::Receiver<InjectJob>,
) {
    loop {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(job) => run_job(&shared, &shutdown, &history_control, job),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn run_job(
    shared: &InjectShared,
    shutdown: &AtomicBool,
    history_control: &HistoryControl,
    job: InjectJob,
) {
    let result = deliver(shutdown, history_control, &job);
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

fn deliver(
    shutdown: &AtomicBool,
    history_control: &HistoryControl,
    job: &InjectJob,
) -> Result<(), String> {
    // Expand snippets here rather than on the main thread: {clipboard} then sees
    // the restored original rather than the previous snippet. History entries
    // bypass expansion so copied text such as "{date}" is pasted verbatim.
    let text = if job.expand_template {
        let clipboard_text = clipboard::read_text();
        let user = std::env::var("USERNAME").unwrap_or_default();
        let host = std::env::var("COMPUTERNAME").unwrap_or_default();
        let context = ExpandContext {
            now: chrono::Local::now(),
            clipboard: clipboard_text.as_deref(),
            user: &user,
            host: &host,
        };
        expand(&job.template, &context)
    } else {
        job.template.clone()
    };
    if text.is_empty() && !job.send_enter {
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
        match is_elevated(target) {
            Some(true) => {
                return Err("无法向管理员权限窗口输入：请以管理员身份运行 Vibekey".to_string())
            }
            Some(false) => {}
            None => log::debug!("could not determine the integrity level of the target window"),
        }
    }

    // A physical Ctrl/Shift/Alt/Win left over from the hotkey would corrupt the
    // pasted text, so wait briefly for the user to let go.
    wait_modifiers_release(shutdown, MODIFIER_RELEASE_TIMEOUT);

    // The hook swallows Alt+<digit>, so the target window may have entered its
    // menu loop on WM_SYSKEYUP(VK_MENU); injected Ctrl+V/Enter would go to the
    // menu instead of the document.
    leave_menu_mode();

    // Check the window that will really receive the input: SetForegroundWindow
    // may have failed, leaving another (possibly elevated) window in front.
    // SAFETY: GetForegroundWindow has no preconditions.
    let receiving = unsafe { GetForegroundWindow() };
    if !receiving.is_null() {
        match is_elevated(receiving) {
            Some(true) => {
                return Err("无法向管理员权限窗口输入：请以管理员身份运行 Vibekey".to_string())
            }
            Some(false) => {}
            None => log::debug!("could not determine the integrity level of the foreground window"),
        }
    }

    match job.input_method {
        InputMethod::Paste => match paste(history_control, job, &text) {
            Ok(()) => Ok(()),
            Err(error) => {
                log::warn!("clipboard paste failed ({error}); falling back to typing");
                type_text(shutdown, job, &text)
            }
        },
        InputMethod::Type => type_text(shutdown, job, &text),
    }
}

/// Whether the process that owns the window runs elevated (high integrity).
///
/// UIPI blocks SendInput from a normal process to an elevated one. Returns None
/// when the token cannot be queried; callers then only log, because being unable
/// to inspect the token does not prove that the target is elevated.
fn is_elevated(hwnd: HWND) -> Option<bool> {
    let mut process_id = 0u32;
    // SAFETY: process_id is a valid out pointer.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    if process_id == 0 {
        return None;
    }
    // SAFETY: the process handle and the token handle are closed again in every
    // path; all buffers have the size that is passed to the API.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return None;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 || token.is_null() {
            CloseHandle(process);
            return None;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut returned = 0u32;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        CloseHandle(process);
        if queried == 0 {
            None
        } else {
            Some(elevation.TokenIsElevated != 0)
        }
    }
}

/// Whether the foreground window's thread is inside a menu loop.
fn foreground_in_menu_mode() -> bool {
    // SAFETY: GetForegroundWindow has no preconditions.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return false;
    }
    let mut process_id = 0u32;
    // SAFETY: process_id is a valid out pointer.
    let thread_id = unsafe { GetWindowThreadProcessId(foreground, &mut process_id) };
    if thread_id == 0 {
        return false;
    }
    let mut info: GUITHREADINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    // SAFETY: info is initialised with its size and the thread id belongs to the
    // foreground window's thread; GetGUIThreadInfo only reads.
    (unsafe { GetGUIThreadInfo(thread_id, &mut info) }) != 0
        && info.flags & (GUI_INMENUMODE | GUI_POPUPMENUMODE) != 0
}

/// Leave the target's menu mode before injecting.
///
/// When the hook swallows Alt+<digit>, the target window only sees the Alt key
/// down/up pair, so DefWindowProc enters its menu loop on WM_SYSKEYUP(VK_MENU).
/// A paste or Enter would then be routed to the menu instead of the document.
fn leave_menu_mode() {
    // Let the target process the Alt key up first: the menu loop is entered by
    // that message, so immediately after the physical release the target may
    // still look like "no menu".
    std::thread::sleep(Duration::from_millis(30));
    if !foreground_in_menu_mode() {
        return;
    }
    log::info!("target window is in menu mode; pressing Escape before injecting");
    let _ = tap(VK_ESCAPE);
    std::thread::sleep(Duration::from_millis(40));
    if foreground_in_menu_mode() {
        log::warn!("target window is still in menu mode after Escape");
    }
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

/// Clipboard based delivery: copy, Ctrl+V, optional Enter and, when requested,
/// restore the previous clipboard text again from this thread.
///
/// The restore is serialised inside the worker: the next job only starts after
/// the previous one has restored the original clipboard text, so a second snippet
/// never saves the text of the first snippet as "the original".
fn paste(history_control: &HistoryControl, job: &InjectJob, text: &str) -> Result<(), String> {
    // Inspect the clipboard once, before changing it: plain text can be restored
    // byte for byte, everything else (files, images, HTML, RTF) must be left
    // alone, in which case the worker types the snippet instead.
    let mut saved_text: Option<String> = None;
    if job.restore_clipboard {
        match clipboard::inspect() {
            Some(clipboard::ClipboardContent::Rich) => {
                log::info!("clipboard holds non text data; typing the snippet instead of pasting");
                return Err("剪贴板包含非文本内容，改为直接输入以保留原剪贴板".to_string());
            }
            Some(clipboard::ClipboardContent::Text(text)) => saved_text = text,
            Some(clipboard::ClipboardContent::Empty) => saved_text = None,
            None => {
                // Without knowing what the clipboard holds, overwriting it could
                // destroy data that cannot be restored.
                log::warn!("could not inspect the clipboard; typing the snippet instead");
                return Err("无法读取剪贴板内容，改为直接输入以保留原剪贴板".to_string());
            }
        }
    }

    if !history_control.internal_write(|| clipboard::write_text(text)) {
        return Err("无法写入剪贴板".to_string());
    }
    std::thread::sleep(Duration::from_millis(50));
    if !press_control_v() {
        return Err("SendInput 发送 Ctrl+V 失败（目标窗口可能以管理员权限运行）".to_string());
    }
    if job.send_enter {
        std::thread::sleep(Duration::from_millis(30));
        if !tap(VK_RETURN) {
            return Err("SendInput 发送回车失败（目标窗口可能以管理员权限运行）".to_string());
        }
    }
    if job.restore_clipboard {
        // Give slow target applications time to read the clipboard before it is
        // rewritten.
        std::thread::sleep(CLIPBOARD_RESTORE_DELAY);
        let restored = history_control.internal_write(|| match saved_text.as_deref() {
            Some(saved) => clipboard::write_text(saved),
            // The clipboard was empty before the paste; restore that state.
            None => clipboard::clear(),
        });
        if !restored {
            // A failed restore must not turn a successful injection into an
            // error; it is only reported in the log.
            log::warn!("could not restore the previous clipboard content");
        }
    }
    Ok(())
}

/// SendInput(KEYEVENTF_UNICODE) based delivery.
fn type_text(shutdown: &AtomicBool, job: &InjectJob, text: &str) -> Result<(), String> {
    let mut count = 0usize;
    for character in text.chars() {
        if shutdown.load(Ordering::Acquire) {
            return Err("正在退出".to_string());
        }
        if character == '\r' {
            continue;
        }
        if character == '\n' {
            if !tap(VK_RETURN) {
                return Err("SendInput 发送回车失败（目标窗口可能以管理员权限运行）".to_string());
            }
        } else {
            let mut buffer = [0u16; 2];
            for unit in character.encode_utf16(&mut buffer) {
                if !send(&[
                    unicode_event(*unit, 0),
                    unicode_event(*unit, KEYEVENTF_KEYUP),
                ]) {
                    return Err(
                        "SendInput 输入字符失败（目标窗口可能以管理员权限运行）".to_string()
                    );
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
