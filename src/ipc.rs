//! Find the running Blurman and tell it the rules file changed.

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, PostMessageW, RegisterWindowMessageW, ASFW_ANY,
};

pub const HOST_CLASS: windows::core::PCWSTR = windows::core::w!("BlurmanHost");

const DETACHED_PROCESS: u32 = 0x0000_0008;

pub fn msg_reload() -> u32 {
    unsafe { RegisterWindowMessageW(windows::core::w!("Blurman.Reload")) }
}

pub fn msg_show() -> u32 {
    unsafe { RegisterWindowMessageW(windows::core::w!("Blurman.Show")) }
}

pub fn msg_shutdown() -> u32 {
    unsafe { RegisterWindowMessageW(windows::core::w!("Blurman.Shutdown")) }
}

pub fn find_host() -> Option<HWND> {
    unsafe { FindWindowW(HOST_CLASS, HOST_CLASS) }
        .ok()
        .filter(|hwnd| !hwnd.0.is_null())
}

pub fn signal(message: u32) -> bool {
    let Some(hwnd) = find_host() else {
        return false;
    };
    unsafe { PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0)).is_ok() }
}

/// Bring the running Blurman window forward. Returns false if none is running.
pub fn show_running() -> bool {
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    signal(msg_show())
}

/// Tell a running Blurman to reload the rules, or open Blurman if none is running.
pub fn reload_or_launch() -> Result<(), String> {
    if signal(msg_reload()) {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    Command::new(exe)
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Could not start Blurman: {err}"))
}
