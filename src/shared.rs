//! State shared by the window, the tray, and the effect thread.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, PostThreadMessageW, SetForegroundWindow, ShowWindowAsync, SW_RESTORE, SW_SHOW,
};

/// A slider change to apply on the panes already tracking `process`. Not a full reload.
#[derive(Clone)]
pub struct Tweak {
    pub process: String,
    pub transparency: u8,
    pub blur: u8,
}

pub struct Shared {
    pub fallback: AtomicBool,
    /// The effect thread's hidden host window.
    pub host: AtomicIsize,
    /// The Blurman window while it is open, so a second launch or the tray can bring it forward.
    pub main_window: AtomicIsize,
    /// The thread that runs the window and the tray, woken with `tray::WM_OPEN`.
    pub ui_thread: AtomicU32,
    /// Set when the window closes into the tray instead of quitting.
    pub to_tray: AtomicBool,
    ctx: Mutex<Option<egui::Context>>,
    /// Bumped whenever the rules file is reloaded, so the window can pick up outside edits.
    pub rules_generation: AtomicU64,
    pub worker_done: AtomicBool,
    status: Mutex<String>,
    /// Slider edits waiting for the effect thread. One slot per process; a newer drag replaces it.
    tweaks: Mutex<Vec<Tweak>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            fallback: AtomicBool::new(false),
            host: AtomicIsize::new(0),
            main_window: AtomicIsize::new(0),
            ui_thread: AtomicU32::new(0),
            to_tray: AtomicBool::new(false),
            ctx: Mutex::new(None),
            rules_generation: AtomicU64::new(0),
            worker_done: AtomicBool::new(false),
            status: Mutex::new(String::new()),
            tweaks: Mutex::new(Vec::new()),
        })
    }

    pub fn push_tweak(&self, tweak: Tweak) {
        let Ok(mut tweaks) = self.tweaks.lock() else {
            return;
        };
        if let Some(existing) = tweaks
            .iter_mut()
            .find(|item| item.process.eq_ignore_ascii_case(&tweak.process))
        {
            *existing = tweak;
        } else {
            tweaks.push(tweak);
        }
    }

    pub fn take_tweaks(&self) -> Vec<Tweak> {
        self.tweaks.lock().map(|mut tweaks| std::mem::take(&mut *tweaks)).unwrap_or_default()
    }

    pub fn set_ctx(&self, ctx: Option<egui::Context>) {
        if let Ok(mut slot) = self.ctx.lock() {
            *slot = ctx;
        }
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
        if let Ok(slot) = self.ctx.lock() {
            if let Some(ctx) = slot.as_ref() {
                ctx.request_repaint();
            }
        }
    }

    /// Bring the Blurman window forward, or open it from the tray. Works from any thread.
    pub fn show_window(&self) {
        let hwnd = crate::target::hwnd_of(self.main_window.load(Ordering::SeqCst));
        if hwnd.0.is_null() {
            unsafe {
                let _ = PostThreadMessageW(
                    self.ui_thread.load(Ordering::SeqCst),
                    crate::tray::WM_OPEN,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            return;
        }
        unsafe {
            let command = if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW };
            let _ = ShowWindowAsync(hwnd, command);
            let _ = SetForegroundWindow(hwnd);
        }
        self.repaint();
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
                WPARAM(0),
                LPARAM(0),
            );
        }
        let start = Instant::now();
        while !self.worker_done.load(Ordering::SeqCst) && start.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
