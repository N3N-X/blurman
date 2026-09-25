//! The per-user Run entry that opens Blurman when you sign in to Windows.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

pub const STARTUP_FLAG: &str = "--startup";

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("Blurman");

pub fn is_enabled() -> bool {
    read().is_some()
}

pub fn set_enabled(on: bool) -> Result<(), String> {
    if !on {
        let err = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) };
        return if err == ERROR_SUCCESS || err == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("Could not turn off start with Windows ({}).", err.0))
        };
    }
    let wide: Vec<u16> = command()?.encode_utf16().chain(std::iter::once(0)).collect();
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            REG_SZ.0,
            Some(wide.as_ptr() as *const std::ffi::c_void),
            (wide.len() * 2) as u32,
        )
    };
    if err == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("Could not turn on start with Windows ({}).", err.0))
    }
}

/// Point an existing entry at this copy of Blurman, in case the exe was moved.
pub fn refresh() {
    if let (Some(current), Ok(wanted)) = (read(), command()) {
        if current != wanted {
            let _ = set_enabled(true);
        }
    }
}

fn command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    Ok(format!("\"{}\" {STARTUP_FLAG}", exe.display()))
}

fn read() -> Option<String> {
    let mut size = 0u32;
    let err = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME, RRF_RT_REG_SZ, None, None, Some(&mut size))
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    let mut buf = vec![0u16; size as usize / 2 + 1];
    let mut size = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            Some(&mut size),
        )
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|ch| *ch == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}
