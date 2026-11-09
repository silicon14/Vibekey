//! The configuration menu shown from the tray icon and from the Alt+Backspace
//! hotkey.

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, PostMessageW, TrackPopupMenu, MF_CHECKED, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_NULL,
};

use crate::util::{self, wide};

pub const CMD_LIST: u32 = 1;
pub const CMD_EDIT_CONFIG: u32 = 2;
pub const CMD_RELOAD: u32 = 3;
pub const CMD_SYNC: u32 = 4;
pub const CMD_SETTINGS: u32 = 5;
pub const CMD_AUTOSTART: u32 = 6;
pub const CMD_OPEN_LOG: u32 = 7;
pub const CMD_ABOUT: u32 = 8;
pub const CMD_EXIT: u32 = 9;
pub const CMD_NEW_SNIPPET: u32 = 10;

/// State that influences the menu entries.
pub struct MenuState {
    pub sync_available: bool,
    pub syncing: bool,
    pub autostart: bool,
}

/// Show the menu at the given screen position and return the selected command.
pub fn show(hwnd: HWND, point: POINT, state: &MenuState) -> Option<u32> {
    // SAFETY: the menu and every string passed to AppendMenuW live for the
    // duration of the call; the menu is destroyed before returning.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return None;
        }
        append(menu, MF_STRING, CMD_LIST, "片段列表…");
        append(menu, MF_STRING, CMD_NEW_SNIPPET, "新建片段…");
        append(menu, MF_STRING, CMD_SETTINGS, "设置与片段管理…");
        append(menu, MF_SEPARATOR, 0, "");
        append(menu, MF_STRING, CMD_EDIT_CONFIG, "编辑 TOML 配置");
        append(menu, MF_STRING, CMD_RELOAD, "重新加载配置");
        let mut sync_flags = MF_STRING;
        if !state.sync_available || state.syncing {
            sync_flags |= MF_GRAYED;
        }
        append(menu, sync_flags, CMD_SYNC, "立即同步");
        append(menu, MF_SEPARATOR, 0, "");
        let mut autostart_flags = MF_STRING;
        if state.autostart {
            autostart_flags |= MF_CHECKED;
        }
        append(menu, autostart_flags, CMD_AUTOSTART, "开机启动");
        append(menu, MF_STRING, CMD_OPEN_LOG, "打开日志文件");
        append(menu, MF_STRING, CMD_ABOUT, "关于 Vibekey");
        append(menu, MF_SEPARATOR, 0, "");
        append(menu, MF_STRING, CMD_EXIT, "退出");

        // AttachThreadInput is needed because this process usually is not the
        // foreground process when the tray icon is clicked; without the
        // foreground the menu ignores the keyboard.
        if !util::force_foreground(hwnd) {
            log::warn!("could not take the foreground for the configuration menu");
        }
        let result = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if result == 0 {
            None
        } else {
            Some(result as u32)
        }
    }
}

/// Append one string entry. The text buffer only has to live for the call
/// because the menu copies it.
unsafe fn append(menu: *mut core::ffi::c_void, flags: u32, id: u32, text: &str) {
    let text = wide(text);
    AppendMenuW(menu, flags, id as usize, text.as_ptr());
}
