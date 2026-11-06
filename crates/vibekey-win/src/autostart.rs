//! Start with Windows support through the per user Run registry key.
//!
//! HKCU\Software\Microsoft\Windows\CurrentVersion\Run is readable and writable
//! without elevation, which is what the tray application needs.

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{GetLastError, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::util::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Vibekey";

/// Full path of the running executable.
pub fn exe_path() -> Option<PathBuf> {
    let mut buffer = [0u16; 1024];
    // SAFETY: the module handle is null (the executable itself) and the buffer
    // is valid for the given length.
    let length = unsafe {
        GetModuleFileNameW(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    if length == 0 || length as usize >= buffer.len() {
        return None;
    }
    Some(PathBuf::from(crate::util::from_wide(
        &buffer[..length as usize],
    )))
}

fn open_run_key(access: u32) -> Option<HKEY> {
    let subkey = wide(RUN_KEY);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: subkey is a valid NUL terminated string and key is a valid out
    // pointer; a null disposition pointer is explicitly allowed.
    let result = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key) };
    if result == ERROR_SUCCESS && !key.is_null() {
        Some(key)
    } else {
        None
    }
}

/// Read the current autostart value.
pub fn is_enabled() -> bool {
    let Some(path) = exe_path() else {
        return false;
    };
    let _ = path;
    let Some(key) = open_run_key(KEY_QUERY_VALUE) else {
        return false;
    };
    let name = wide(VALUE_NAME);
    let mut value_type = 0u32;
    let mut buffer = [0u8; 2048];
    let mut size = buffer.len() as u32;
    // SAFETY: all pointers refer to live stack memory of the documented types.
    let result = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            buffer.as_mut_ptr(),
            &mut size,
        )
    };
    // SAFETY: key came from RegOpenKeyExW and is closed exactly once.
    unsafe { RegCloseKey(key) };
    result == ERROR_SUCCESS && value_type == REG_SZ && size > 2
}

/// Enable or disable starting with Windows. The command line is the quoted full
/// path of the executable.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        let path = exe_path().ok_or_else(|| "无法获取程序路径".to_string())?;
        let command = format!("\"{}\"", path.display());
        let subkey = wide(RUN_KEY);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: subkey is a valid NUL terminated string and key is a valid out
        // pointer; a null disposition pointer is explicitly allowed.
        let result = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        if result != ERROR_SUCCESS || key.is_null() {
            return Err(format!("无法打开注册表项（错误 {result}）"));
        }
        let name = wide(VALUE_NAME);
        let data = wide(&command);
        let bytes = data.len() * std::mem::size_of::<u16>();
        // SAFETY: name and data are valid NUL terminated UTF-16 buffers, the
        // key is open for writing and the byte count matches the data length.
        let result = unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                bytes as u32,
            )
        };
        // SAFETY: key was opened above and is closed exactly once.
        unsafe { RegCloseKey(key) };
        if result == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("无法写入开机启动项（错误 {result}）"))
        }
    } else {
        let Some(key) = open_run_key(KEY_SET_VALUE) else {
            // No key means no autostart entry.
            return Ok(());
        };
        let name = wide(VALUE_NAME);
        // SAFETY: name is a valid NUL terminated string and the key is open.
        let result = unsafe { RegDeleteValueW(key, name.as_ptr()) };
        // SAFETY: key came from RegOpenKeyExW and is closed exactly once.
        unsafe { RegCloseKey(key) };
        if result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("无法删除开机启动项（错误 {result}）"))
        }
    }
}

/// Refresh the registry entry from the configuration on startup.
pub fn reconcile(enabled_in_config: bool) {
    if enabled_in_config && !is_enabled() {
        if let Err(error) = set_enabled(true) {
            log::warn!("could not recreate the autostart entry: {error}");
        }
    }
}

/// Avoid "unused import" noise on GetLastError while keeping the import local.
#[allow(dead_code)]
fn _last_error() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    unsafe { GetLastError() }
}
