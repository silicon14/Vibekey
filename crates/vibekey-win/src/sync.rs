//! Background synchronisation thread.
//!
//! The thread owns the ureq agent (which is not cheap to build) and runs one
//! synchronisation step when the periodic interval elapses, when the user asks
//! for one, or when the main thread requests it after a configuration change.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use vibekey_core::sync::{SyncEngine, SyncError, SyncOutcome, SyncProtection, UreqWebDav};
use vibekey_core::{Config, CryptoError, SecretKind, SecretStore, SyncState};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::util::{isize_to_hwnd, WM_APP_RELOAD, WM_APP_SYNC_DONE};

/// Interval used when the configuration disables the periodic sync.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Configuration values the sync thread needs.
#[derive(Clone)]
pub struct SyncSettings {
    pub enabled: bool,
    pub url: String,
    pub username: String,
    pub interval_minutes: u32,
    pub backup_on_conflict: bool,
    pub encrypt: bool,
    pub timeout_secs: u64,
    pub config_path: PathBuf,
}

impl SyncSettings {
    pub fn from_config(config: &Config, config_path: PathBuf) -> Self {
        Self {
            enabled: config.sync.enabled,
            url: config.sync.url.clone(),
            username: config.sync.username.clone(),
            interval_minutes: config.sync.interval_minutes,
            backup_on_conflict: config.sync.backup_on_conflict,
            encrypt: config.sync.encrypt,
            timeout_secs: config.sync.timeout_secs,
            config_path,
        }
    }

    fn usable(&self) -> bool {
        self.enabled && !self.url.trim().is_empty()
    }

    /// Periodic sync interval; None when interval_minutes is 0, which means
    /// "only synchronise manually or after a configuration change".
    fn periodic(&self) -> Option<Duration> {
        if self.interval_minutes == 0 {
            None
        } else {
            Some(Duration::from_secs(self.interval_minutes as u64 * 60))
        }
    }
}

/// Commands sent from the main thread to the sync thread.
pub enum SyncCommand {
    /// New configuration values; the interval is recomputed.
    Snapshot(Box<SyncSettings>),
    /// Run a synchronisation step now.
    SyncNow { manual: bool },
    /// Stop the thread.
    Shutdown,
}

/// Result of one synchronisation step, consumed by the main window.
pub struct SyncResult {
    pub manual: bool,
    pub ok: bool,
    pub pulled: bool,
    pub message: String,
}

/// State shared with the main window.
pub struct SyncShared {
    main_hwnd: AtomicIsize,
    result: Mutex<Option<SyncResult>>,
    /// True while a synchronisation step (possibly a blocking HTTP request) is
    /// running. Used to avoid joining the thread during process exit.
    busy: AtomicBool,
}

impl SyncShared {
    pub fn take_result(&self) -> Option<SyncResult> {
        self.result.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// Handle to the sync thread.
pub struct SyncHandle {
    sender: Option<mpsc::Sender<SyncCommand>>,
    shared: Arc<SyncShared>,
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl SyncHandle {
    pub fn shared(&self) -> &Arc<SyncShared> {
        &self.shared
    }

    pub fn update(&self, settings: SyncSettings) {
        self.send(SyncCommand::Snapshot(Box::new(settings)));
    }

    pub fn sync_now(&self, manual: bool) {
        self.send(SyncCommand::SyncNow { manual });
    }

    fn send(&self, command: SyncCommand) {
        if let Some(sender) = self.sender.as_ref() {
            if sender.send(command).is_err() {
                log::warn!("the sync thread is gone");
            }
        }
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.send(SyncCommand::Shutdown);
        self.sender = None;
        if let Some(join) = self.join.take() {
            // A running HTTP request cannot be interrupted. Give it a very short
            // grace period and detach afterwards, so that an unresponsive server
            // never blocks the application exit.
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.shared.busy.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if self.shared.busy.load(Ordering::Acquire) {
                log::warn!("a synchronisation is still running; detaching the sync thread");
                drop(join);
            } else {
                let _ = join.join();
            }
        }
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the sync thread.
pub fn start(
    main_hwnd: windows_sys::Win32::Foundation::HWND,
    secrets: Arc<dyn SecretStore>,
) -> SyncHandle {
    let shared = Arc::new(SyncShared {
        main_hwnd: AtomicIsize::new(crate::util::hwnd_to_isize(main_hwnd)),
        result: Mutex::new(None),
        busy: AtomicBool::new(false),
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel::<SyncCommand>();
    let worker_shared = Arc::clone(&shared);
    let worker_shutdown = Arc::clone(&shutdown);
    let join = std::thread::Builder::new()
        .name("vibekey-sync".to_string())
        .spawn(move || worker(worker_shared, worker_shutdown, secrets, receiver))
        .ok();
    SyncHandle {
        sender: Some(sender),
        shared,
        shutdown,
        join,
    }
}

fn worker(
    shared: Arc<SyncShared>,
    shutdown: Arc<AtomicBool>,
    secrets: Arc<dyn SecretStore>,
    receiver: mpsc::Receiver<SyncCommand>,
) {
    let mut settings: Option<SyncSettings> = None;
    let mut interval = DEFAULT_INTERVAL;
    let mut next_due: Option<Instant> = None;

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let wait = match next_due {
            Some(due) => due
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(50)),
            None => Duration::from_secs(60),
        };
        match receiver.recv_timeout(wait) {
            Ok(SyncCommand::Snapshot(new)) => {
                let enabled = new.usable();
                let periodic = new.periodic();
                interval = periodic.unwrap_or(DEFAULT_INTERVAL);
                settings = Some(*new);
                next_due = if enabled {
                    periodic.map(|period| Instant::now() + period)
                } else {
                    None
                };
            }
            Ok(SyncCommand::SyncNow { manual }) => {
                if let Some(current) = settings.clone() {
                    run_step(&shared, &secrets, &current, manual);
                } else if manual {
                    finish(
                        &shared,
                        SyncResult {
                            manual,
                            ok: false,
                            pulled: false,
                            message: "同步设置尚未加载".to_string(),
                        },
                    );
                }
                if next_due.is_some() {
                    next_due = Some(Instant::now() + interval);
                }
            }
            Ok(SyncCommand::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let (Some(current), Some(due)) = (settings.clone(), next_due) {
                    if Instant::now() >= due {
                        run_step(&shared, &secrets, &current, false);
                        next_due = Some(Instant::now() + interval);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Run one synchronisation step, keeping the busy flag up to date so that
/// SyncHandle::stop can decide between joining and detaching.
fn run_step(
    shared: &SyncShared,
    secrets: &Arc<dyn SecretStore>,
    settings: &SyncSettings,
    manual: bool,
) {
    shared.busy.store(true, Ordering::Release);
    run_sync(shared, secrets, settings, manual);
    shared.busy.store(false, Ordering::Release);
}

fn run_sync(
    shared: &SyncShared,
    secrets: &Arc<dyn SecretStore>,
    settings: &SyncSettings,
    manual: bool,
) {
    if !settings.usable() {
        if manual {
            finish(
                shared,
                SyncResult {
                    manual,
                    ok: false,
                    pulled: false,
                    message: "同步未启用或未填写 WebDAV 地址".to_string(),
                },
            );
        }
        return;
    }
    let password = match secrets.load(SecretKind::WebDav) {
        Ok(Some(password)) => password,
        Ok(None) if settings.username.is_empty() => String::new(),
        Ok(None) => {
            finish(
                shared,
                SyncResult {
                    manual,
                    ok: false,
                    pulled: false,
                    message: "未设置 WebDAV 密码，请在设置中填写".to_string(),
                },
            );
            return;
        }
        Err(error) => {
            finish(
                shared,
                SyncResult {
                    manual,
                    ok: false,
                    pulled: false,
                    message: format!("无法读取已保存的 WebDAV 密码：{error}"),
                },
            );
            return;
        }
    };
    // Capture one coherent password pair for this entire run. Conditional
    // rotation commit below prevents an older run from clearing a newer pair.
    let (encryption_current, encryption_previous) = match secrets.load_encryption_passwords() {
        Ok(pair) => pair,
        Err(error) => {
            finish(
                shared,
                SyncResult {
                    manual,
                    ok: false,
                    pulled: false,
                    message: format!("无法读取配置加密密码及轮换状态：{error}"),
                },
            );
            return;
        }
    };
    if settings.encrypt && encryption_current.is_none() {
        finish(
            shared,
            SyncResult {
                manual,
                ok: false,
                pulled: false,
                message: "已启用配置加密，请在设置中填写独立同步密码".to_string(),
            },
        );
        return;
    }
    let protection = match SyncProtection::new(
        settings.encrypt,
        encryption_current.clone(),
        encryption_previous.clone(),
    ) {
        Ok(protection) => protection,
        Err(error) => {
            finish(
                shared,
                SyncResult {
                    manual,
                    ok: false,
                    pulled: false,
                    message: sync_error_message(&SyncError::Crypto(error)),
                },
            );
            return;
        }
    };

    let timeout = Duration::from_secs(settings.timeout_secs.max(1));
    let dav = UreqWebDav::new(&settings.username, &password, timeout);
    let engine = SyncEngine::new(
        &dav,
        settings.url.clone(),
        settings.config_path.clone(),
        settings.backup_on_conflict,
    )
    .with_protection(protection);
    let state_path = SyncState::path_for(&settings.config_path);
    let mut state = SyncState::load(&state_path);
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();

    match engine.sync(&mut state, &timestamp) {
        Ok(outcome) => {
            if let Err(error) = state.save(&state_path) {
                let message =
                    format!("同步内容已处理，但状态保存失败：{error}。请重试；旧加密密码已保留");
                log::warn!("could not save the sync state: {error}");
                finish(
                    shared,
                    SyncResult {
                        manual,
                        ok: false,
                        pulled: false,
                        message,
                    },
                );
                return;
            }
            if let Some(current) = encryption_current.as_deref() {
                if let Err(error) = secrets.finish_password_rotation(
                    SecretKind::ConfigEncryption,
                    current,
                    encryption_previous.as_deref(),
                ) {
                    // The new remote is already usable. Retaining the previous
                    // password is safe and the next successful sync retries it.
                    log::warn!("could not finish encryption password rotation: {error}");
                }
            }
            let (pulled, message) = match outcome {
                SyncOutcome::UpToDate => (false, "配置已是最新".to_string()),
                SyncOutcome::Pushed { first_upload } => (
                    false,
                    if first_upload {
                        "已上传配置".to_string()
                    } else {
                        "已上传修改".to_string()
                    },
                ),
                SyncOutcome::Pulled => (true, "已从远端更新配置".to_string()),
                SyncOutcome::ConflictLocalWins { remote_backup } => (
                    false,
                    match remote_backup {
                        Some(url) => format!("同步冲突：本地优先，远端内容已备份到 {url}"),
                        None => "同步冲突：本地优先".to_string(),
                    },
                ),
            };
            log::info!("sync: {message}");
            if pulled {
                post(shared, WM_APP_RELOAD, 0);
            }
            finish(
                shared,
                SyncResult {
                    manual,
                    ok: true,
                    pulled,
                    message,
                },
            );
        }
        Err(error) => {
            let message = sync_error_message(&error);
            log::warn!("sync failed: {error}");
            state.record_error(error.to_string());
            if let Err(error) = state.save(&state_path) {
                log::warn!("could not save the sync state: {error}");
            }
            finish(
                shared,
                SyncResult {
                    manual,
                    ok: false,
                    pulled: false,
                    message,
                },
            );
        }
    }
}

fn sync_error_message(error: &SyncError) -> String {
    match error {
        SyncError::Crypto(CryptoError::PasswordRequired) => {
            "同步失败：启用配置加密后必须设置独立同步密码".to_string()
        }
        SyncError::Crypto(CryptoError::PasswordTooShort) => {
            "同步失败：配置加密密码至少需要 8 个字符".to_string()
        }
        SyncError::Crypto(CryptoError::Decrypt) => {
            "同步失败：配置加密密码错误，或远端加密内容已被篡改；本地配置未修改".to_string()
        }
        SyncError::Crypto(CryptoError::InvalidFormat) => {
            "同步失败：远端加密配置格式无效或版本不受支持；本地配置未修改".to_string()
        }
        SyncError::Crypto(CryptoError::Random | CryptoError::Encrypt) => {
            "同步失败：系统无法安全加密配置，请重试".to_string()
        }
        SyncError::EncryptionRequired => {
            "同步失败：远端已加密，请先在本机启用配置加密并成功同步。只有主动关闭已确认的加密状态才能迁移为明文"
                .to_string()
        }
        SyncError::Downgrade => {
            "同步失败：远端此前已加密，现在却被替换为明文。已阻止 HTTP 降级，未覆盖任何配置"
                .to_string()
        }
        _ => format!("同步失败：{error}"),
    }
}

fn finish(shared: &SyncShared, result: SyncResult) {
    if let Ok(mut slot) = shared.result.lock() {
        *slot = Some(result);
    }
    post(shared, WM_APP_SYNC_DONE, 0);
}

fn post(shared: &SyncShared, message: u32, wparam: usize) {
    let hwnd = isize_to_hwnd(shared.main_hwnd.load(Ordering::Acquire));
    if hwnd.is_null() {
        return;
    }
    // SAFETY: the handle belongs to the main window and the message is one of
    // our private WM_APP messages.
    unsafe {
        PostMessageW(hwnd, message, wparam, 0);
    }
}
