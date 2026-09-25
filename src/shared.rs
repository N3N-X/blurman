//! State shared by the window, the tray, and the effect thread.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, SetForegroundWindow, ShowWindowAsync, SW_HIDE, SW_RESTORE, SW_SHOW,
};

pub struct Shared {
    pub fallback: AtomicBool,
    /// The effect thread's hidden host window.
    pub host: AtomicIsize,
    /// The Blurman window, so a second launch or the tray can bring it forward.
    pub main_window: AtomicIsize,
    pub ctx: OnceLock<egui::Context>,
    /// Bumped whenever the rules file is reloaded, so the window can pick up outside edits.
    pub rules_generation: AtomicU64,
    pub worker_done: AtomicBool,
    status: Mutex<String>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            fallback: AtomicBool::new(false),
            host: AtomicIsize::new(0),
            main_window: AtomicIsize::new(0),
            ctx: OnceLock::new(),
            rules_generation: AtomicU64::new(0),
            worker_done: AtomicBool::new(false),
            status: Mutex::new(String::new()),
        })
    }

    pub fn set_status(&self, text: impl Into<String>) {
        if let Ok(mut slot) = self.status.lock() {
            *slot = text.into();
        }
        self.repaint();
    }

    pub fn status(&self) -> String {
        self.status.lock().map(|text| text.clone()).unwrap_or_default()
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    /// Show and focus the Blurman window. egui does not run frames while its window is hidden,
    /// so this goes through Win32 directly and works from any thread.
    pub fn show_window(&self) {
        let hwnd = crate::target::hwnd_of(self.main_window.load(Ordering::SeqCst));
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let command = if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW };
            let _ = ShowWindowAsync(hwnd, command);
            let _ = SetForegroundWindow(hwnd);
        }
        self.repaint();
    }

    pub fn hide_window(&self) {
        let hwnd = crate::target::hwnd_of(self.main_window.load(Ordering::SeqCst));
        if !hwnd.0.is_null() {
            unsafe {
                let _ = ShowWindowAsync(hwnd, SW_HIDE);
            }
        }
    }

    /// Ask the effect thread to put every app back, and wait for it to finish.
    pub fn shutdown_worker(&self, timeout: Duration) {
        let hwnd = crate::target::hwnd_of(self.host.load(Ordering::SeqCst));
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                crate::ipc::msg_shutdown(),
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        let start = Instant::now();
        while !self.worker_done.load(Ordering::SeqCst) && start.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
