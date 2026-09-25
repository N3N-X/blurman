//! Owns the glass windows. Every WinRT and HWND call for the effect stays on this thread.

use crate::glass::{GlassPane, GlassSession};
use crate::ipc::{self, msg_reload, msg_show, msg_shutdown, HOST_CLASS};
use crate::mapping::{self, SavedStyle};
use crate::rules::{self, PersistedWindow, Rule, Settings, Store};
use crate::shared::Shared;
use crate::solid::{Renderer, SolidView, WM_FRAME};
use crate::target::{self, LiveWindow};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    RegisterClassExW, SetTimer, TranslateMessage, CHILDID_SELF, EVENT_OBJECT_CLOAKED,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, MSG,
    OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

/// How often to look for new windows of ruled apps. Moves and z-order changes arrive as events.
const SCAN_MS: u32 = 250;
const SCAN_TIMER: usize = 1;

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

/// Runs `f` on this thread's engine. Skipped if the engine is already busy, which happens when
/// a window event arrives while a cross-process call is waiting; the next scan catches up.
fn with_engine(f: impl FnOnce(&mut Engine)) {
    ENGINE.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            if let Some(engine) = slot.as_mut() {
                f(engine);
            }
        }
    });
}

struct Tracked {
    pid: u32,
    process_start: u64,
    process: String,
    saved: SavedStyle,
    transparency: u8,
    blur: u8,
    pane: GlassPane,
    solid_text: bool,
    /// Live capture of the app while solid text is on.
    view: Option<SolidView>,
    /// The real window is at `HIDDEN_ALPHA` and the glass shows its copy.
    hidden: bool,
}

impl Tracked {
    /// Back to the plain fade: make the app visible first so it never blinks out.
    fn stop_solid(&mut self, hwnd: HWND) -> Result<(), String> {
        let result = if std::mem::take(&mut self.hidden) {
            target::apply_alpha(hwnd, self.transparency, false)
        } else {
            Ok(())
        };
        self.view = None;
        self.pane.hide_content();
        result
    }

    fn follow(&mut self, hwnd: HWND) {
        if target::is_out_of_view(hwnd) {
            self.pane.hide();
        } else {
            self.pane
                .place_under(hwnd, target::frame_bounds(hwnd), target::is_topmost(hwnd));
        }
    }
}

struct Engine {
    shared: Arc<Shared>,
    host: HWND,
    glass: GlassSession,
    /// Created the first time an app asks for solid text.
    renderer: Option<Renderer>,
    store: Store,
    settings: Settings,
    /// A fullscreen game is in front, so solid text is paused.
    gaming: bool,
    watchdog: bool,
    tracked: HashMap<isize, Tracked>,
    /// Original styles remembered from an earlier run, keyed by HWND.
    prior: HashMap<isize, PersistedWindow>,
    /// Windows that refused the effect, so they are not retried on every scan.
    refused: HashMap<isize, (u32, u64)>,
    persisted: Vec<PersistedWindow>,
}

pub fn run(shared: Arc<Shared>) {
    let host = match create_host() {
        Ok(hwnd) => hwnd,
        Err(err) => {
            shared.set_status(err);
            shared.worker_done.store(true, Ordering::SeqCst);
            return;
        }
    };
    let engine = Engine::new(shared.clone(), host);
    ENGINE.with(|slot| *slot.borrow_mut() = Some(engine));
    with_engine(Engine::reload);
    let hooks = install_hooks();
    unsafe {
        let _ = SetTimer(Some(host), SCAN_TIMER, SCAN_MS, None);
    }
    shared.host.store(host.0 as isize, Ordering::SeqCst);

    let reload = msg_reload();
    let show = msg_show();
    let shutdown = msg_shutdown();
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.hwnd == host {
            if message.message == WM_FRAME {
                with_engine(|engine| engine.on_frame(message.wParam.0 as isize));
            } else if message.message == WM_TIMER {
                with_engine(Engine::reconcile);
            } else if message.message == reload {
                with_engine(Engine::reload);
            } else if message.message == show {
                shared.show_window();
            } else if message.message == shutdown {
                break;
            }
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    for hook in hooks {
        unsafe {
            let _ = UnhookWinEvent(hook);
        }
    }
    with_engine(Engine::restore_all);
    ENGINE.with(|slot| slot.borrow_mut().take());
    shared.host.store(0, Ordering::SeqCst);
    unsafe {
        let _ = DestroyWindow(host);
    }
    shared.worker_done.store(true, Ordering::SeqCst);
}

/// Put back every window a crashed or killed Blurman left faded. Used when no Blurman is running.
pub fn restore_leftovers() {
    for saved in rules::load_state() {
        if target::window_alive(saved.hwnd, saved.pid, saved.process_start) {
            target::restore_alpha(target::hwnd_of(saved.hwnd), &saved.saved_style());
        }
    }
    let _ = rules::save_state(&[]);
}

/// Wait for the Blurman with process id `parent` to exit, then put back anything it left.
/// Solid text leaves apps almost invisible, so this covers Blurman being killed.
pub fn watch(parent: u32) {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, parent) else {
            return;
        };
        WaitForSingleObject(process, INFINITE);
        let _ = CloseHandle(process);
    }
    // A Blurman that exits normally has already cleaned up; a new one owns the state file.
    std::thread::sleep(Duration::from_millis(500));
    if ipc::find_host().is_none() {
        restore_leftovers();
    }
}

impl Engine {
    fn new(shared: Arc<Shared>, host: HWND) -> Self {
        let prior: HashMap<isize, PersistedWindow> = rules::load_state()
            .into_iter()
            .map(|window| (window.hwnd, window))
            .collect();
        let mut persisted: Vec<PersistedWindow> = prior.values().cloned().collect();
        persisted.sort_by_key(|window| window.hwnd);
        Self {
            shared,
            host,
            glass: GlassSession::new(),
            renderer: None,
            store: Store::default(),
            settings: Settings::default(),
            gaming: false,
            watchdog: false,
            tracked: HashMap::new(),
            prior,
            refused: HashMap::new(),
            persisted,
        }
    }

    fn reload(&mut self) {
        self.store = rules::load();
        self.settings = rules::load_settings();
        self.refused.clear();
        self.shared.set_status("");
        self.shared.rules_generation.fetch_add(1, Ordering::SeqCst);
        self.reconcile();
    }

    fn reconcile(&mut self) {
        if self.store.paused {
            self.restore_all();
            return;
        }
        self.check_gaming();

        let wanted = target::windows_for_rules(&self.store.rules);
        let wanted_ids: HashSet<isize> = wanted.iter().map(|(window, _)| window.hwnd).collect();

        let stale: Vec<isize> = self
            .tracked
            .keys()
            .copied()
            .filter(|hwnd| !wanted_ids.contains(hwnd))
            .collect();
        for hwnd in stale {
            self.drop_tracked(hwnd);
        }

        let stale_prior: Vec<isize> = self
            .prior
            .keys()
            .copied()
            .filter(|hwnd| !wanted_ids.contains(hwnd))
            .collect();
        for hwnd in stale_prior {
            if let Some(saved) = self.prior.remove(&hwnd) {
                if target::window_alive(saved.hwnd, saved.pid, saved.process_start) {
                    target::restore_alpha(target::hwnd_of(saved.hwnd), &saved.saved_style());
                }
            }
        }

        self.refused.retain(|hwnd, _| wanted_ids.contains(hwnd));
        for (window, rule) in wanted {
            let reused = self.tracked.get(&window.hwnd).is_some_and(|tracked| {
                tracked.pid != window.pid || tracked.process_start != window.process_start
            });
            if reused {
                self.drop_tracked(window.hwnd);
            }
            if let Some(tracked) = self.tracked.get_mut(&window.hwnd) {
                if sync_tracked(tracked, &window, &rule, &self.shared, self.renderer.as_ref()) {
                    self.start_solid(window.hwnd);
                }
                continue;
            }
            if self.refused.get(&window.hwnd) == Some(&(window.pid, window.process_start)) {
                continue;
            }
            let key = (window.hwnd, window.pid, window.process_start);
            if let Err(err) = self.attach(window, &rule) {
                self.shared.set_status(err);
                self.refused.insert(key.0, (key.1, key.2));
            }
        }
        self.shared
            .fallback
            .store(self.glass.fallback(), Ordering::SeqCst);
        // Catches frames whose notice arrived while the engine was busy.
        let solid: Vec<isize> = self.tracked.iter().filter(|(_, t)| t.view.is_some()).map(|(k, _)| *k).collect();
        for key in solid {
            self.on_frame(key);
        }
        self.persist();
    }

    /// Pause solid text while a fullscreen game is in front, unless the user opted in.
    fn check_gaming(&mut self) {
        let gaming = !self.settings.solid_text_in_fullscreen && target::fullscreen_in_front();
        if gaming == self.gaming {
            return;
        }
        self.gaming = gaming;
        let solid: Vec<isize> = self.tracked.iter().filter(|(_, t)| t.solid_text).map(|(k, _)| *k).collect();
        for key in solid {
            if gaming {
                if let Some(tracked) = self.tracked.get_mut(&key) {
                    if let Err(err) = tracked.stop_solid(target::hwnd_of(key)) {
                        self.shared.set_status(format!("{}: {err}", tracked.process));
                    }
                }
            } else {
                self.start_solid(key);
            }
        }
    }

    fn start_solid(&mut self, key: isize) {
        if self.gaming || self.tracked.get(&key).is_none_or(|t| !t.solid_text || t.view.is_some()) {
            return;
        }
        if self.renderer.is_none() {
            match Renderer::new() {
                Ok(renderer) => self.renderer = Some(renderer),
                Err(err) => {
                    self.shared.set_status(format!("Solid text is unavailable: {err}"));
                    return;
                }
            }
        }
        let (Some(renderer), Some(tracked)) = (self.renderer.as_ref(), self.tracked.get_mut(&key)) else {
            return;
        };
        let Some(compositor) = self.glass.compositor() else {
            self.shared
                .set_status("Solid text needs the adjustable blur, which this PC does not have.");
            return;
        };
        let started = SolidView::start(renderer, compositor, target::hwnd_of(key), self.host).and_then(
            |(view, surface)| {
                tracked.pane.show_content(compositor, &surface)?;
                Ok(view)
            },
        );
        match started {
            Ok(view) => {
                tracked.view = Some(view);
                if !self.watchdog {
                    self.watchdog = ipc::spawn_watchdog();
                }
            }
            Err(err) => self.shared.set_status(format!("{}: {err}", tracked.process)),
        }
    }

    /// Draw the app's newest frame on its glass. The real window is hidden only once its copy
    /// is on screen, so the app never disappears.
    fn on_frame(&mut self, key: isize) {
        let (Some(renderer), Some(tracked)) = (self.renderer.as_ref(), self.tracked.get_mut(&key)) else {
            return;
        };
        let Some(view) = tracked.view.as_mut() else {
            return;
        };
        let hwnd = target::hwnd_of(key);
        let drawn = view
            .draw(renderer, mapping::background_opacity(tracked.transparency), false)
            .and_then(|drawn| {
                if drawn && !tracked.hidden {
                    target::set_alpha(hwnd, mapping::HIDDEN_ALPHA, false)?;
                    tracked.hidden = true;
                }
                Ok(())
            });
        if let Err(err) = drawn {
            self.shared.set_status(format!("{}: solid text stopped. {err}", tracked.process));
            let _ = tracked.stop_solid(hwnd);
        }
    }

    fn attach(&mut self, window: LiveWindow, rule: &Rule) -> Result<(), String> {
        if window.elevated {
            return Err(format!(
                "{} runs as administrator. Run Blurman as administrator to frost it.",
                window.process
            ));
        }
        let hwnd = target::hwnd_of(window.hwnd);
        let saved = self
            .prior
            .remove(&window.hwnd)
            .filter(|saved| saved.pid == window.pid && saved.process_start == window.process_start)
            .map(|saved| saved.saved_style())
            .unwrap_or_else(|| target::capture_style(hwnd));
        let pane = self.glass.open(window.bounds, rule.blur)?;
        if let Err(err) = target::apply_alpha(hwnd, rule.transparency, !saved.was_layered) {
            pane.close();
            return Err(format!("{}: {err}", window.process));
        }
        let mut tracked = Tracked {
            pid: window.pid,
            process_start: window.process_start,
            process: window.process,
            saved,
            transparency: rule.transparency,
            blur: rule.blur,
            pane,
            solid_text: rule.solid_text,
            view: None,
            hidden: false,
        };
        tracked.follow(hwnd);
        self.tracked.insert(window.hwnd, tracked);
        self.start_solid(window.hwnd);
        Ok(())
    }

    fn on_event(&mut self, event: u32, hwnd: HWND) {
        let key = hwnd.0 as isize;
        match event {
            EVENT_OBJECT_DESTROY => {
                if self.tracked.contains_key(&key) {
                    self.drop_tracked(key);
                    self.persist();
                }
            }
            EVENT_SYSTEM_FOREGROUND => {
                self.check_gaming();
                for (hwnd, tracked) in &mut self.tracked {
                    tracked.follow(target::hwnd_of(*hwnd));
                }
            }
            // A new window of a ruled app is frosted the moment it appears, is restored, or gets
            // its title, instead of on the next scan.
            EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED | EVENT_OBJECT_NAMECHANGE | EVENT_SYSTEM_MINIMIZEEND
                if !self.tracked.contains_key(&key) =>
            {
                if self.store.paused || self.refused.contains_key(&key) {
                    return;
                }
                let ruled = target::frostable_process(hwnd).is_some_and(|process| {
                    self.store
                        .rules
                        .iter()
                        .any(|rule| rule.enabled && rule.process.eq_ignore_ascii_case(&process))
                });
                if ruled {
                    self.reconcile();
                }
            }
            _ => {
                if let Some(tracked) = self.tracked.get_mut(&key) {
                    tracked.follow(hwnd);
                }
            }
        }
    }

    fn drop_tracked(&mut self, hwnd: isize) {
        if let Some(tracked) = self.tracked.remove(&hwnd) {
            if target::window_alive(hwnd, tracked.pid, tracked.process_start) {
                target::restore_alpha(target::hwnd_of(hwnd), &tracked.saved);
            }
            tracked.pane.close();
        }
    }

    fn restore_all(&mut self) {
        let keys: Vec<isize> = self.tracked.keys().copied().collect();
        for hwnd in keys {
            self.drop_tracked(hwnd);
        }
        for (_, saved) in self.prior.drain() {
            if target::window_alive(saved.hwnd, saved.pid, saved.process_start) {
                target::restore_alpha(target::hwnd_of(saved.hwnd), &saved.saved_style());
            }
        }
        self.persist();
    }

    /// Record original styles so a crash can be undone on the next start. Writes only on change.
    fn persist(&mut self) {
        let mut windows: Vec<PersistedWindow> = self
            .tracked
            .iter()
            .map(|(hwnd, tracked)| PersistedWindow {
                hwnd: *hwnd,
                pid: tracked.pid,
                process_start: tracked.process_start,
                process: tracked.process.clone(),
                original_exstyle: tracked.saved.exstyle,
                original_layered: tracked.saved.was_layered,
                original_alpha: tracked.saved.alpha,
            })
            .chain(self.prior.values().cloned())
            .collect();
        windows.sort_by_key(|window| window.hwnd);
        if windows != self.persisted && rules::save_state(&windows).is_ok() {
            self.persisted = windows;
        }
    }
}

/// Apply rule edits to a window we already track. Returns true when solid text should start.
fn sync_tracked(
    tracked: &mut Tracked,
    window: &LiveWindow,
    rule: &Rule,
    shared: &Shared,
    renderer: Option<&Renderer>,
) -> bool {
    let hwnd = target::hwnd_of(window.hwnd);
    if tracked.transparency != rule.transparency {
        // A hidden window keeps its near-zero alpha; the slider only changes the drawn copy.
        let applied = if tracked.hidden {
            Ok(())
        } else {
            target::apply_alpha(hwnd, rule.transparency, false)
        };
        match applied {
            Ok(()) => tracked.transparency = rule.transparency,
            Err(err) => shared.set_status(format!("{}: {err}", window.process)),
        }
        if let (Some(view), Some(renderer)) = (tracked.view.as_mut(), renderer) {
            let opacity = mapping::background_opacity(tracked.transparency);
            if let Err(err) = view.draw(renderer, opacity, true) {
                shared.set_status(format!("{}: {err}", window.process));
            }
        }
    }
    let mut start_solid = false;
    if tracked.solid_text != rule.solid_text {
        tracked.solid_text = rule.solid_text;
        if rule.solid_text {
            start_solid = true;
        } else if let Err(err) = tracked.stop_solid(hwnd) {
            shared.set_status(format!("{}: {err}", window.process));
        }
    }
    if tracked.blur != rule.blur {
        match tracked.pane.set_blur(rule.blur) {
            Ok(()) => tracked.blur = rule.blur,
            Err(err) => shared.set_status(format!("{}: {err}", window.process)),
        }
    }
    tracked.follow(hwnd);
    start_solid
}

fn install_hooks() -> Vec<HWINEVENTHOOK> {
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    ranges
        .iter()
        .map(|&(first, last)| unsafe {
            SetWinEventHook(
                first,
                last,
                None,
                Some(on_win_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        })
        .filter(|hook| !hook.is_invalid())
        .collect()
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.0.is_null() || id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    with_engine(|engine| engine.on_event(event, hwnd));
}

fn create_host() -> Result<HWND, String> {
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|err| err.to_string())?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(host_proc),
        hInstance: instance.into(),
        lpszClassName: HOST_CLASS,
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&class);
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            HOST_CLASS,
            HOST_CLASS,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }
    .map_err(|err| format!("Could not create the Blurman host window: {err}"))
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
