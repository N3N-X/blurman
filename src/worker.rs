//! Owns the glass windows. Every WinRT and HWND call for the effect stays on this thread.

use crate::glass::{GlassPane, GlassSession};
use crate::ipc::{msg_reload, msg_show, msg_shutdown, HOST_CLASS};
use crate::mapping::SavedStyle;
use crate::rules::{self, PersistedWindow, Rule, Store};
use crate::shared::Shared;
use crate::target::{self, LiveWindow};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    RegisterClassExW, SetTimer, TranslateMessage, CHILDID_SELF, EVENT_OBJECT_CLOAKED,
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, MSG,
    OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

/// How often to look for new windows of ruled apps. Moves and z-order changes arrive as events.
const SCAN_MS: u32 = 250;
const SCAN_TIMER: usize = 1;
/// How long a window faded at creation may go without becoming a normal app window before it
/// is put back. Covers hidden helper windows and splash screens.
const EARLY_WAIT: Duration = Duration::from_secs(3);

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    /// Window events that arrived while the engine was busy, as (event, HWND).
    static EVENTS: RefCell<VecDeque<(u32, isize)>> = const { RefCell::new(VecDeque::new()) };
}

/// Runs `f` on this thread's engine, then any window events that arrived meanwhile. Skipped if
/// the engine is already busy, which happens when a call is re-entered while a cross-process
/// call is waiting; the next scan catches up.
fn with_engine(f: impl FnOnce(&mut Engine)) {
    let ran = ENGINE.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return false;
        };
        if let Some(engine) = slot.as_mut() {
            f(engine);
        }
        true
    });
    if ran {
        drain_events();
    }
}

/// Window events arrive in the middle of cross-process calls, when the engine is busy. They
/// wait here instead of being dropped, so a window created in that moment is still faded.
fn queue_event(event: u32, hwnd: isize) {
    EVENTS.with(|events| {
        let mut events = events.borrow_mut();
        if !events.contains(&(event, hwnd)) {
            events.push_back((event, hwnd));
        }
    });
    drain_events();
}

fn drain_events() {
    ENGINE.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return;
        };
        let Some(engine) = slot.as_mut() else {
            EVENTS.with(|events| events.borrow_mut().clear());
            return;
        };
        while let Some((event, hwnd)) = EVENTS.with(|events| events.borrow_mut().pop_front()) {
            engine.on_event(event, target::hwnd_of(hwnd));
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
}

impl Tracked {
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
    glass: GlassSession,
    store: Store,
    tracked: HashMap<isize, Tracked>,
    /// Windows of ruled apps faded the moment they were created, waiting to be shown and titled
    /// so they can get their glass. Keeps the original style and when the fade went on.
    early: HashMap<isize, (PersistedWindow, Instant)>,
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
    let engine = Engine::new(shared.clone());
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
            if message.message == WM_TIMER {
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

impl Engine {
    fn new(shared: Arc<Shared>) -> Self {
        let prior: HashMap<isize, PersistedWindow> = rules::load_state()
            .into_iter()
            .map(|window| (window.hwnd, window))
            .collect();
        let mut persisted: Vec<PersistedWindow> = prior.values().cloned().collect();
        persisted.sort_by_key(|window| window.hwnd);
        Self {
            shared,
            glass: GlassSession::new(),
            store: Store::default(),
            tracked: HashMap::new(),
            early: HashMap::new(),
            prior,
            refused: HashMap::new(),
            persisted,
        }
    }

    fn reload(&mut self) {
        self.store = rules::load();
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
                sync_tracked(tracked, &window, &rule, &self.shared);
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
        let now = Instant::now();
        let expired: Vec<isize> = self
            .early
            .iter()
            .filter(|(hwnd, (_, since))| !wanted_ids.contains(hwnd) && now - *since > EARLY_WAIT)
            .map(|(hwnd, _)| *hwnd)
            .collect();
        for hwnd in expired {
            self.drop_early(hwnd);
        }
        self.shared
            .fallback
            .store(self.glass.fallback(), Ordering::SeqCst);
        self.persist();
    }

    fn attach(&mut self, window: LiveWindow, rule: &Rule) -> Result<(), String> {
        if window.elevated {
            return Err(format!(
                "{} runs as administrator. Run Blurman as administrator to frost it.",
                window.process
            ));
        }
        let hwnd = target::hwnd_of(window.hwnd);
        let saved = self.original_style(window.hwnd, window.pid, window.process_start);
        let pane = match self.glass.open(window.bounds, rule.blur) {
            Ok(pane) => pane,
            Err(err) => {
                target::restore_alpha(hwnd, &saved);
                return Err(err);
            }
        };
        if let Err(err) = target::apply_alpha(hwnd, rule.transparency, !saved.was_layered) {
            pane.close();
            target::restore_alpha(hwnd, &saved);
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
        };
        tracked.follow(hwnd);
        self.tracked.insert(window.hwnd, tracked);
        Ok(())
    }

    /// The style `hwnd` had before any Blurman touched it: from the early fade, from an earlier
    /// run, or as it is now.
    fn original_style(&mut self, hwnd: isize, pid: u32, process_start: u64) -> SavedStyle {
        let same = |saved: &PersistedWindow| saved.pid == pid && saved.process_start == process_start;
        let early = self.early.remove(&hwnd).map(|(saved, _)| saved);
        let prior = self.prior.remove(&hwnd);
        early
            .filter(same)
            .or(prior.filter(same))
            .map(|saved| saved.saved_style())
            .unwrap_or_else(|| target::capture_style(target::hwnd_of(hwnd)))
    }

    /// Fade a window of a ruled app right away, before it is shown or titled, so it never
    /// appears solid first. The glass follows once it qualifies as an app window.
    fn fade_early(&mut self, hwnd: HWND) -> bool {
        if target::is_minimized(hwnd) {
            return false;
        }
        let Some(pid) = target::early_candidate(hwnd) else {
            return false;
        };
        let Some(process) = target::process_name(pid) else {
            return false;
        };
        let Some(transparency) = self
            .store
            .rules
            .iter()
            .find(|rule| rule.enabled && rule.process.eq_ignore_ascii_case(&process))
            .map(|rule| rule.transparency)
        else {
            return false;
        };
        let key = hwnd.0 as isize;
        let process_start = target::process_start(pid);
        let saved = self.original_style(key, pid, process_start);
        let nudge = !saved.was_layered && !target::is_out_of_view(hwnd);
        if target::apply_alpha(hwnd, transparency, nudge).is_err() {
            return false;
        }
        let window = PersistedWindow::new(key, pid, process_start, process, &saved);
        self.early.insert(key, (window, Instant::now()));
        self.persist();
        true
    }

    fn on_event(&mut self, event: u32, hwnd: HWND) {
        let key = hwnd.0 as isize;
        match event {
            EVENT_OBJECT_DESTROY => {
                if self.tracked.contains_key(&key) {
                    self.drop_tracked(key);
                    self.persist();
                } else if self.early.remove(&key).is_some() {
                    self.persist();
                }
            }
            EVENT_SYSTEM_FOREGROUND => {
                for (hwnd, tracked) in &mut self.tracked {
                    tracked.follow(target::hwnd_of(*hwnd));
                }
            }
            // New windows of ruled apps are faded as they are created and frosted the moment
            // they appear, are restored, or get their title, instead of on the next scan.
            EVENT_OBJECT_CREATE
            | EVENT_OBJECT_SHOW
            | EVENT_OBJECT_UNCLOAKED
            | EVENT_OBJECT_NAMECHANGE
            | EVENT_SYSTEM_MINIMIZEEND
                if !self.tracked.contains_key(&key) =>
            {
                if self.store.paused
                    || self.refused.contains_key(&key)
                    || !self.store.rules.iter().any(|rule| rule.enabled)
                {
                    return;
                }
                let faded = self.early.contains_key(&key)
                    || (event != EVENT_OBJECT_NAMECHANGE && self.fade_early(hwnd));
                let ready = if faded {
                    target::is_frostable(hwnd)
                } else {
                    target::frostable_process(hwnd).is_some_and(|process| {
                        self.store
                            .rules
                            .iter()
                            .any(|rule| rule.enabled && rule.process.eq_ignore_ascii_case(&process))
                    })
                };
                if ready {
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

    fn drop_early(&mut self, hwnd: isize) {
        if let Some((saved, _)) = self.early.remove(&hwnd) {
            if target::window_alive(hwnd, saved.pid, saved.process_start) {
                target::restore_alpha(target::hwnd_of(hwnd), &saved.saved_style());
            }
        }
    }

    fn restore_all(&mut self) {
        let keys: Vec<isize> = self.tracked.keys().copied().collect();
        for hwnd in keys {
            self.drop_tracked(hwnd);
        }
        let keys: Vec<isize> = self.early.keys().copied().collect();
        for hwnd in keys {
            self.drop_early(hwnd);
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
            .map(|(hwnd, tracked)| {
                PersistedWindow::new(*hwnd, tracked.pid, tracked.process_start, tracked.process.clone(), &tracked.saved)
            })
            .chain(self.early.values().map(|(saved, _)| saved.clone()))
            .chain(self.prior.values().cloned())
            .collect();
        windows.sort_by_key(|window| window.hwnd);
        if windows != self.persisted && rules::save_state(&windows).is_ok() {
            self.persisted = windows;
        }
    }
}

fn sync_tracked(tracked: &mut Tracked, window: &LiveWindow, rule: &Rule, shared: &Shared) {
    let hwnd = target::hwnd_of(window.hwnd);
    if tracked.transparency != rule.transparency {
        match target::apply_alpha(hwnd, rule.transparency, false) {
            Ok(()) => tracked.transparency = rule.transparency,
            Err(err) => shared.set_status(format!("{}: {err}", window.process)),
        }
    }
    if tracked.blur != rule.blur {
        match tracked.pane.set_blur(rule.blur) {
            Ok(()) => tracked.blur = rule.blur,
            Err(err) => shared.set_status(format!("{}: {err}", window.process)),
        }
    }
    tracked.follow(hwnd);
}

fn install_hooks() -> Vec<HWINEVENTHOOK> {
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
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
    queue_event(event, hwnd.0 as isize);
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
