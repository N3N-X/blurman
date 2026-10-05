//! Find normal top-level windows and fade or restore them.

use crate::mapping::{self, RestoreAction, SavedStyle};
use crate::rules::Rule;
use std::collections::HashMap;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, COLORREF, FILETIME, HANDLE, HWND, LPARAM, RECT,
    WIN32_ERROR,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_TRANSITIONS_FORCEDISABLED, DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, RedrawWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetLayeredWindowAttributes, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible, IsZoomed, SetLayeredWindowAttributes, SetWindowLongPtrW,
    SetWindowPos, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, WS_CAPTION, WS_CHILD, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_THICKFRAME,
};

#[derive(Debug, Clone)]
pub struct LiveWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub process_start: u64,
    pub process: String,
    pub title: String,
    pub elevated: bool,
    /// On another virtual desktop, or hidden by the shell.
    pub cloaked: bool,
    /// Covers the whole monitor. A maximized window stops above the taskbar, so it is not fullscreen.
    pub fullscreen: bool,
    pub bounds: RECT,
}

#[derive(Debug, Clone)]
pub struct AppGroup {
    pub process: String,
    pub sample_title: String,
    pub windows: usize,
    pub elevated: bool,
    /// Every visible window of this app covers its monitor.
    pub fullscreen: bool,
}

pub fn list_groups() -> Vec<AppGroup> {
    let mut windows: Vec<LiveWindow> = visible_windows().into_iter().filter(|window| !window.cloaked).collect();
    // The list only needs the admin badge. Start times are for the effect thread.
    enrich(&mut windows, false, true);
    let mut groups: Vec<AppGroup> = Vec::new();
    for window in windows {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.process.eq_ignore_ascii_case(&window.process))
        {
            group.windows += 1;
            group.elevated |= window.elevated;
            group.fullscreen &= window.fullscreen;
        } else {
            groups.push(AppGroup {
                process: window.process,
                sample_title: window.title,
                windows: 1,
                elevated: window.elevated,
                fullscreen: window.fullscreen,
            });
        }
    }
    groups.sort_by_key(|group| group.process.to_ascii_lowercase());
    groups
}

/// Windows a rule wants frosted, plus rules whose open windows are all fullscreen.
pub struct RuledWindows {
    pub wanted: Vec<(LiveWindow, Rule)>,
    pub fullscreen_only: Vec<String>,
}

pub fn windows_for_rules(rules: &[Rule]) -> RuledWindows {
    let enabled: Vec<&Rule> = rules.iter().filter(|rule| rule.enabled).collect();
    if enabled.is_empty() {
        return RuledWindows {
            wanted: Vec::new(),
            fullscreen_only: Vec::new(),
        };
    }
    let mut windows = visible_windows();
    windows.retain(|window| {
        enabled
            .iter()
            .any(|rule| rule.process.eq_ignore_ascii_case(&window.process))
    });
    // OpenProcess only for the executables that have a rule, not every window on the desktop.
    enrich(&mut windows, true, true);
    let mut wanted = Vec::new();
    let mut fullscreen_only = Vec::new();
    for rule in enabled {
        let mut any = false;
        let mut any_normal = false;
        for window in windows
            .iter()
            .filter(|window| rule.process.eq_ignore_ascii_case(&window.process))
        {
            any = true;
            if window.fullscreen {
                continue;
            }
            any_normal = true;
            wanted.push((window.clone(), rule.clone()));
        }
        if any && !any_normal {
            fullscreen_only.push(rule.process.clone());
        }
    }
    RuledWindows { wanted, fullscreen_only }
}

struct ProcessInfo {
    start: u64,
    elevated: bool,
}

/// Visible top-level windows with process names filled in. Does not open any process.
fn visible_windows() -> Vec<LiveWindow> {
    let mut found: Vec<LiveWindow> = Vec::new();
    unsafe {
        let param = LPARAM(&mut found as *mut Vec<LiveWindow> as isize);
        let _ = EnumWindows(Some(enum_proc), param);
    }
    let own = unsafe { GetCurrentProcessId() };
    found.retain(|window| window.pid != own);
    if found.is_empty() {
        return found;
    }
    let names = process_names();
    for window in &mut found {
        window.process = names
            .get(&window.pid)
            .cloned()
            .unwrap_or_else(|| format!("pid-{}", window.pid));
    }
    found
}

/// Fill start time and elevation for the processes already in `windows`. One open per process.
fn enrich(windows: &mut [LiveWindow], start: bool, elevated: bool) {
    if windows.is_empty() || (!start && !elevated) {
        return;
    }
    let mut info: HashMap<u32, ProcessInfo> = HashMap::new();
    for window in windows {
        let process = info.entry(window.pid).or_insert_with(|| ProcessInfo {
            start: if start { process_start(window.pid) } else { 0 },
            elevated: if elevated { is_elevated(window.pid) } else { false },
        });
        if start {
            window.process_start = process.start;
        }
        if elevated {
            window.elevated = process.elevated;
        }
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let sink = &mut *(lparam.0 as *mut Vec<LiveWindow>);
    if let Some(window) = inspect_window(hwnd) {
        sink.push(window);
    }
    BOOL(1)
}

fn inspect_window(hwnd: HWND) -> Option<LiveWindow> {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        if GetAncestor(hwnd, GA_ROOT) != hwnd {
            return None;
        }
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if exstyle & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        let class = class_name(hwnd);
        if matches!(
            class.as_str(),
            "Progman" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "WorkerW"
        ) {
            return None;
        }
        let title = window_text(hwnd);
        if title.trim().is_empty() {
            return None;
        }
        let bounds = frame_bounds(hwnd);
        let iconic = IsIconic(hwnd).as_bool();
        // A minimized window keeps its glass hidden but stays faded, so it restores already frosted.
        if !iconic && (bounds.right - bounds.left < 160 || bounds.bottom - bounds.top < 90) {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let cloaked = is_cloaked(hwnd);
        Some(LiveWindow {
            hwnd: hwnd.0 as isize,
            pid,
            process_start: 0,
            process: String::new(),
            title,
            elevated: false,
            cloaked,
            fullscreen: !iconic && !cloaked && covers_its_monitor(hwnd, bounds),
            bounds,
        })
    }
}

/// How far a window edge may sit inside the monitor and still count as covering it.
const MONITOR_SLOP: i32 = 8;

/// True when every edge of `bounds` reaches the monitor, within [`MONITOR_SLOP`].
/// A maximized window stops at the taskbar, so its bottom (or side) falls short of `rcMonitor`.
pub(crate) fn covers_monitor(bounds: RECT, monitor: RECT) -> bool {
    bounds.left <= monitor.left + MONITOR_SLOP
        && bounds.top <= monitor.top + MONITOR_SLOP
        && bounds.right >= monitor.right - MONITOR_SLOP
        && bounds.bottom >= monitor.bottom - MONITOR_SLOP
}

fn covers_its_monitor(hwnd: HWND, bounds: RECT) -> bool {
    monitor_rect(hwnd).is_some_and(|monitor| covers_monitor(bounds, monitor))
}

fn monitor_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        Some(info.rcMonitor)
    }
}

/// Visible, not minimized, and covering the monitor it is on.
pub fn is_fullscreen(hwnd: HWND) -> bool {
    unsafe {
        if IsIconic(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() || is_cloaked(hwnd) {
            return false;
        }
    }
    covers_its_monitor(hwnd, frame_bounds(hwnd))
}

/// The window's corner preference, or None where Windows has no such attribute (Windows 10).
pub fn corner_preference(hwnd: HWND) -> Option<i32> {
    unsafe {
        let mut preference = 0i32;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &mut preference as *mut i32 as *mut std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        )
        .ok()?;
        Some(preference)
    }
}

pub fn frame_bounds(hwnd: HWND) -> RECT {
    unsafe {
        let mut rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut std::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_ok()
            && rect.right > rect.left
            && rect.bottom > rect.top
        {
            return rect;
        }
        let _ = GetWindowRect(hwnd, &mut rect);
        rect
    }
}

pub fn is_cloaked(hwnd: HWND) -> bool {
    unsafe {
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
    }
}

/// Minimized, hidden, or cloaked: the glass should not be on screen.
pub fn is_out_of_view(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() || is_cloaked(hwnd) }
}

pub fn is_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

pub fn is_topmost(hwnd: HWND) -> bool {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 }
}

fn class_name(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..copied as usize])
    }
}

/// The executable name behind `hwnd`, if it is a window that could be frosted now.
pub fn frostable_process(hwnd: HWND) -> Option<String> {
    process_name(inspect_window(hwnd)?.pid)
}

pub fn is_frostable(hwnd: HWND) -> bool {
    inspect_window(hwnd).is_some()
}

/// The process id behind a top-level window with a title bar or sizing border and a real size.
/// Judged without needing the window to be visible or titled, so it works the moment the window
/// is created. Menus, tooltips, and hidden helper windows are left alone.
pub fn early_candidate(hwnd: HWND) -> Option<u32> {
    unsafe {
        if GetAncestor(hwnd, GA_ROOT) != hwnd {
            return None;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if style & WS_CHILD.0 != 0 || exstyle & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        if style & WS_CAPTION.0 != WS_CAPTION.0 && style & WS_THICKFRAME.0 == 0 {
            return None;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() || rect.right - rect.left < 160 || rect.bottom - rect.top < 90 {
            return None;
        }
        // A window created already covering the monitor is a fullscreen app. Leave it solid.
        if covers_its_monitor(hwnd, rect) {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

pub fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let named =
            QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        let path = String::from_utf16_lossy(&path[..len as usize]);
        named.then(|| path.rsplit('\\').next().unwrap_or(&path).to_string())
    }
}

fn process_names() -> HashMap<u32, String> {
    let mut names = HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return names;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|ch| *ch == 0)
                    .unwrap_or(entry.szExeFile.len());
                names.insert(
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..len]),
                );
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    names
}

pub fn process_start(pid: u32) -> u64 {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return 0;
        };
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let stamp = if GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user).is_ok() {
            ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64
        } else {
            0
        };
        let _ = CloseHandle(handle);
        stamp
    }
}

/// True when the process runs above our privilege level, so its windows will refuse changes.
/// Anti-cheat and other protected processes deny full access without being elevated, so this
/// reads the token instead of testing what can be opened.
fn is_elevated(pid: u32) -> bool {
    static SELF_ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *SELF_ELEVATED.get_or_init(|| token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)) {
        return false;
    }
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        // An elevated token cannot be opened from a normal process, which is itself the answer.
        let elevated = token_elevated(process).unwrap_or(true);
        let _ = CloseHandle(process);
        elevated
    }
}

fn token_elevated(process: HANDLE) -> Option<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let result = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        result.ok().map(|()| elevation.TokenIsElevated != 0)
    }
}

pub fn hwnd_of(value: isize) -> HWND {
    HWND(value as *mut std::ffi::c_void)
}

pub fn capture_style(hwnd: HWND) -> SavedStyle {
    unsafe {
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as i64;
        let was_layered = exstyle & WS_EX_LAYERED.0 as i64 != 0;
        let mut alpha = 255u8;
        if was_layered {
            let mut key = COLORREF::default();
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();
            let _ = GetLayeredWindowAttributes(hwnd, Some(&mut key), Some(&mut alpha), Some(&mut flags));
            if flags & LWA_ALPHA != LWA_ALPHA {
                alpha = 255;
            }
        }
        SavedStyle {
            exstyle,
            was_layered,
            alpha,
        }
    }
}

pub fn apply_alpha(hwnd: HWND, transparency: u8, nudge: bool) -> Result<(), String> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err("The window is gone.".into());
        }
        if IsHungAppWindow(hwnd).as_bool() {
            return Err("The app is not responding.".into());
        }
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let layered = style | WS_EX_LAYERED.0 as isize;
        if style != layered {
            SetLastError(WIN32_ERROR(0));
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, layered);
            let err = GetLastError();
            if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_LAYERED.0 as isize == 0 {
                return Err(format!(
                    "Windows refused the transparency change ({}). Apps running as administrator need Blurman running as administrator too.",
                    err.0
                ));
            }
            if nudge && !IsZoomed(hwnd).as_bool() {
                nudge_resize(hwnd);
            }
        }
        let alpha = mapping::transparency_to_alpha(transparency);
        SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA).map_err(|err| err.to_string())?;
        // A faded window keeps a picture of itself. Without this, that picture stays painted
        // on top for a frame after you switch away.
        set_transitions_disabled(hwnd, true);
        Ok(())
    }
}

fn set_transitions_disabled(hwnd: HWND, disabled: bool) {
    let disabled = BOOL::from(disabled);
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const BOOL as *const std::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        )
    };
}

/// Some apps paint black after gaining the layered bit until they are resized.
fn nudge_resize(hwnd: HWND) {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let flags = SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED;
        let _ = SetWindowPos(hwnd, None, 0, 0, width + 1, height, flags);
        let _ = SetWindowPos(hwnd, None, 0, 0, width, height, flags);
    }
}

pub fn restore_alpha(hwnd: HWND, saved: &SavedStyle) {
    unsafe {
        // Style changes wait on the target's message loop, so a hung app would hang us too.
        if !IsWindow(Some(hwnd)).as_bool() || IsHungAppWindow(hwnd).as_bool() {
            return;
        }
        set_transitions_disabled(hwnd, false);
        match mapping::restore_action(saved) {
            RestoreAction::KeepLayered { alpha } => {
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
            }
            RestoreAction::ClearLayered => {
                let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as isize));
                let _ = RedrawWindow(
                    Some(hwnd),
                    None,
                    None,
                    RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    #[test]
    fn fullscreen_covers_the_monitor_and_maximized_does_not() {
        let monitor = rect(0, 0, 1920, 1080);
        assert!(covers_monitor(rect(0, 0, 1920, 1080), monitor));
        assert!(covers_monitor(rect(-4, -4, 1924, 1084), monitor));
        // A few pixels short on every edge still counts. The taskbar gap does not.
        assert!(covers_monitor(rect(2, 2, 1916, 1074), monitor));
        assert!(!covers_monitor(rect(0, 0, 1920, 1040), monitor));
        assert!(!covers_monitor(rect(100, 100, 1100, 800), monitor));
    }

    #[test]
    fn a_window_on_a_second_monitor_uses_that_monitor() {
        let monitor = rect(1920, 0, 3840, 1080);
        assert!(covers_monitor(rect(1920, 0, 3840, 1080), monitor));
        assert!(!covers_monitor(rect(0, 0, 1920, 1080), monitor));
    }
}

/// The HWND still belongs to the same process instance we saw earlier.
pub fn window_alive(hwnd: isize, pid: u32, process_start: u64) -> bool {
    let hwnd = hwnd_of(hwnd);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        let mut current = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut current));
        current == pid && (process_start == 0 || self::process_start(pid) == process_start)
    }
}
