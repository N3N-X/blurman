//! Owns the glass windows. Every WinRT and HWND call for the effect stays on this thread.

use crate::glass::{GlassPane, GlassSession};
use crate::ipc::{msg_reload, msg_show, msg_shutdown, msg_tweak, HOST_CLASS};
use crate::mapping::{self, BlurStyle, SavedStyle};
use crate::rules::{self, PersistedWindow, Rule, Store};
use crate::shared::{Shared, Tweak};
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetAncestor, GetMessageW,
    GetWindow, KillTimer, RegisterClassExW, SetTimer, TranslateMessage, CHILDID_SELF,
    EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW,
    EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MENUPOPUPEND,
    EVENT_SYSTEM_MENUPOPUPSTART, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, GA_ROOT,
    GW_OWNER, MSG, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_TIMER,
    WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Backstop while a window is waiting to become a normal app window. Moves arrive as events.
const SCAN_FAST_MS: u32 = 250;
/// Backstop once nothing is waiting. Events still move, show, and restack the glass.
const SCAN_IDLE_MS: u32 = 1000;
const SCAN_TIMER: usize = 1;
/// Coalesce state.json writes so a burst of new windows is one write.
const PERSIST_TIMER: usize = 2;
const PERSIST_MS: u32 = 150;
/// Focus is reported before Windows finishes the z-order change. Restack again once it has.
const SETTLE_TIMER: usize = 3;
const SETTLE_MS: u32 = 100;
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
    style: BlurStyle,
    pane: GlassPane,
    /// A menu is open, so the window is solid and the glass is hidden.
    menu_suspended: bool,
}

impl Tracked {
    /// Returns whether the glass was restacked. A pane already under the window is left alone.
    fn follow(&mut self, hwnd: HWND) -> bool {
        if self.menu_suspended || target::is_out_of_view(hwnd) {
            self.pane.hide();
            false
        } else {
            self.pane
                .place_under(hwnd, target::frame_bounds(hwnd), target::is_topmost(hwnd))
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
    /// Latest state.json contents waiting out the coalesce window.
    pending_state: Option<Vec<PersistedWindow>>,
    persist_armed: bool,
    /// Timer interval last handed to SetTimer. Zero until the host window exists.
    scan_ms: u32,
    /// A one-shot restack is waiting, so focus that landed early gets another pass.
    settle_armed: bool,
    /// Rules currently skipped because every window is fullscreen, and the status that says so.
    fullscreen_apps: Vec<String>,
    fullscreen_status: Option<String>,
    /// Open menu popup -> the frosted window that owns it.
    open_menus: HashMap<isize, isize>,
    /// Nested Win32 menus. The window stays solid until this returns to zero.
    menu_depth: HashMap<isize, u32>,
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
    shared.host.store(host.0 as isize, Ordering::SeqCst);
    // The first reconcile ran before the host existed, so it could not arm the scan timer.
    with_engine(Engine::update_scan_rate);

    let reload = msg_reload();
    let show = msg_show();
    let shutdown = msg_shutdown();
    let tweak = msg_tweak();
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.hwnd == host {
            if message.message == WM_TIMER && message.wParam.0 == SCAN_TIMER {
                with_engine(Engine::reconcile);
            } else if message.message == WM_TIMER && message.wParam.0 == PERSIST_TIMER {
                with_engine(Engine::flush_persist);
            } else if message.message == WM_TIMER && message.wParam.0 == SETTLE_TIMER {
                with_engine(Engine::settle);
            } else if message.message == reload {
                with_engine(Engine::reload);
            } else if message.message == show {
                shared.show_window();
            } else if message.message == tweak {
                with_engine(Engine::apply_tweak);
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
            pending_state: None,
            persist_armed: false,
            scan_ms: 0,
            settle_armed: false,
            fullscreen_apps: Vec::new(),
            fullscreen_status: None,
            open_menus: HashMap::new(),
            menu_depth: HashMap::new(),
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
            self.note_fullscreen(Vec::new());
            self.restore_all();
            self.update_scan_rate();
            return;
        }

        let scan = target::windows_for_rules(&self.store.rules);
        self.note_fullscreen(scan.fullscreen_only);
        let wanted = scan.wanted;
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
            if self.tracked.contains_key(&window.hwnd) {
                self.sync_existing(&window, &rule);
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
        self.persist(false);
        self.update_scan_rate();
    }

    /// Say which ruled apps are covering the screen. Clears that note when they stop, and leaves
    /// a different status (an attach error) in place.
    fn note_fullscreen(&mut self, names: Vec<String>) {
        if names == self.fullscreen_apps {
            return;
        }
        let previous = self.fullscreen_status.take();
        self.fullscreen_apps = names;
        let message = match self.fullscreen_apps.as_slice() {
            [] => None,
            [one] => Some(format!("{one} is fullscreen, so Blurman left it alone.")),
            many => Some(format!(
                "{} are fullscreen, so Blurman left them alone.",
                many.join(", ")
            )),
        };
        if let Some(message) = message {
            self.fullscreen_status = Some(message.clone());
            self.shared.set_status(message);
            return;
        }
        if let Some(previous) = previous {
            if self.shared.status() == previous {
                self.shared.set_status("");
            }
        }
    }

    fn update_scan_rate(&mut self) {
        let ms = if self.early.is_empty() {
            SCAN_IDLE_MS
        } else {
            SCAN_FAST_MS
        };
        if self.scan_ms == ms {
            return;
        }
        let hwnd = target::hwnd_of(self.shared.host.load(Ordering::SeqCst));
        if hwnd.0.is_null() {
            return;
        }
        self.scan_ms = ms;
        unsafe {
            let _ = SetTimer(Some(hwnd), SCAN_TIMER, ms, None);
        }
    }

    /// Apply slider edits onto the panes already up. The rules file is saved by the window,
    /// so this does not enumerate or reload.
    fn apply_tweak(&mut self) {
        for tweak in self.shared.take_tweaks() {
            self.apply_one_tweak(tweak);
        }
    }

    fn apply_one_tweak(&mut self, tweak: Tweak) {
        let applied = {
            let Some(rule) = self
                .store
                .rules
                .iter_mut()
                .find(|rule| rule.process.eq_ignore_ascii_case(&tweak.process))
            else {
                return;
            };
            rule.transparency = mapping::clamp_transparency(tweak.transparency);
            rule.blur = mapping::clamp_blur(tweak.blur);
            rule.style = tweak.style;
            if !rule.enabled || self.store.paused {
                None
            } else {
                Some((
                    rule.transparency,
                    rule.blur,
                    rule.style,
                    rule.process.clone(),
                ))
            }
        };
        let Some((transparency, blur, style, process)) = applied else {
            return;
        };
        let keys: Vec<isize> = self
            .tracked
            .iter()
            .filter(|(_, tracked)| tracked.process.eq_ignore_ascii_case(&process))
            .map(|(hwnd, _)| *hwnd)
            .collect();
        for key in keys {
            self.apply_to(key, transparency, blur, style);
        }
    }

    fn apply_to(&mut self, key: isize, transparency: u8, blur: u8, style: BlurStyle) {
        let Engine {
            glass,
            tracked,
            shared,
            ..
        } = self;
        let Some(tracked) = tracked.get_mut(&key) else {
            return;
        };
        apply_values(
            tracked,
            target::hwnd_of(key),
            transparency,
            blur,
            style,
            glass,
            shared,
        );
    }

    fn sync_existing(&mut self, window: &LiveWindow, rule: &Rule) {
        let Engine {
            glass,
            tracked,
            shared,
            ..
        } = self;
        let Some(tracked) = tracked.get_mut(&window.hwnd) else {
            return;
        };
        let hwnd = target::hwnd_of(window.hwnd);
        apply_values(
            tracked,
            hwnd,
            rule.transparency,
            rule.blur,
            rule.style,
            glass,
            shared,
        );
        tracked.follow(hwnd);
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
        let pane = match self.glass.open(window.bounds, rule.blur, rule.style) {
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
            style: rule.style,
            pane,
            menu_suspended: false,
        };
        tracked.follow(hwnd);
        self.tracked.insert(window.hwnd, tracked);
        Ok(())
    }

    /// The style `hwnd` had before any Blurman touched it: from the early fade, from an earlier
    /// run, or as it is now.
    fn original_style(&mut self, hwnd: isize, pid: u32, process_start: u64) -> SavedStyle {
        let same =
            |saved: &PersistedWindow| saved.pid == pid && saved.process_start == process_start;
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
        self.persist(false);
        // A window is waiting, so the slow idle scan is not enough.
        self.update_scan_rate();
        true
    }

    fn on_event(&mut self, event: u32, hwnd: HWND) {
        // A menu on a layered window is drawn with that window's alpha, and the glass pane can
        // sit on top of it. Hold the frost off until the menu closes.
        if event == EVENT_SYSTEM_MENUPOPUPSTART {
            self.note_menu_start(hwnd);
            return;
        }
        if event == EVENT_SYSTEM_MENUPOPUPEND {
            self.note_menu_end(hwnd);
            return;
        }
        if event == EVENT_OBJECT_SHOW && target::is_menu_popup(hwnd) {
            self.note_menu_window(hwnd);
            return;
        }
        if event == EVENT_OBJECT_HIDE || event == EVENT_OBJECT_DESTROY {
            self.note_menu_window_gone(hwnd);
        }
        let key = hwnd.0 as isize;
        match event {
            EVENT_OBJECT_DESTROY => {
                if self.tracked.contains_key(&key) {
                    self.drop_tracked(key);
                    self.persist(false);
                } else if self.early.remove(&key).is_some() {
                    self.persist(false);
                    self.update_scan_rate();
                }
            }
            EVENT_SYSTEM_FOREGROUND => {
                self.dismiss_menus_unless(hwnd);
                self.stack_foreground(hwnd);
                self.arm_settle();
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
                if !self.tracked.contains_key(&key) {
                    return;
                }
                if target::is_fullscreen(hwnd) {
                    self.drop_tracked(key);
                    self.persist(false);
                } else if let Some(tracked) = self.tracked.get_mut(&key) {
                    tracked.follow(hwnd);
                }
            }
        }
    }

    /// Put the newly focused window's glass behind it with no DWM queries. The settle timer
    /// fixes size afterward. Querying frames here is what left a sharp frame on screen.
    fn stack_foreground(&mut self, hwnd: HWND) {
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        let target = if root.0.is_null() { hwnd } else { root };
        let key = target.0 as isize;
        if let Some(tracked) = self.tracked.get_mut(&key) {
            if tracked.menu_suspended {
                tracked.pane.hide();
            } else {
                tracked.pane.stack_under(target);
            }
        }
    }

    /// Put every glass pane back under its window, using the window's live frame.
    /// Fullscreen is left to move events and the scan. Checking it here drops the effect during
    /// the focus animation, and the blur only comes back on the next scan.
    fn follow_changed(&mut self) {
        let keys: Vec<isize> = self.tracked.keys().copied().collect();
        for key in keys {
            let window = target::hwnd_of(key);
            if let Some(tracked) = self.tracked.get_mut(&key) {
                tracked.follow(window);
            }
        }
    }

    /// Schedule the second restack. SetTimer resets an armed timer, so a burst of focus changes
    /// settles once, after the last one.
    fn arm_settle(&mut self) {
        let hwnd = target::hwnd_of(self.shared.host.load(Ordering::SeqCst));
        if hwnd.0.is_null() {
            return;
        }
        self.settle_armed = true;
        unsafe {
            let _ = SetTimer(Some(hwnd), SETTLE_TIMER, SETTLE_MS, None);
        }
    }

    fn settle(&mut self) {
        if self.settle_armed {
            self.settle_armed = false;
            let hwnd = target::hwnd_of(self.shared.host.load(Ordering::SeqCst));
            if !hwnd.0.is_null() {
                unsafe {
                    let _ = KillTimer(Some(hwnd), SETTLE_TIMER);
                }
            }
        }
        self.sweep_menus();
        self.follow_changed();
    }

    /// The window a menu belongs to, if that window is frosted.
    fn frosted_owner(&self, hwnd: HWND) -> Option<isize> {
        let key = hwnd.0 as isize;
        if self.tracked.contains_key(&key) {
            return Some(key);
        }
        let owner = unsafe { GetWindow(hwnd, GW_OWNER).ok() }?;
        let owner_key = owner.0 as isize;
        if self.tracked.contains_key(&owner_key) {
            return Some(owner_key);
        }
        let root = unsafe { GetAncestor(owner, GA_ROOT) };
        let root_key = root.0 as isize;
        self.tracked.contains_key(&root_key).then_some(root_key)
    }

    fn note_menu_start(&mut self, hwnd: HWND) {
        let Some(owner) = self.frosted_owner(hwnd) else {
            return;
        };
        let depth = self.menu_depth.entry(owner).or_insert(0);
        *depth = depth.saturating_add(1);
        self.suspend_owner(owner);
    }

    fn note_menu_end(&mut self, hwnd: HWND) {
        let Some(owner) = self.frosted_owner(hwnd) else {
            return;
        };
        let done = match self.menu_depth.get_mut(&owner) {
            Some(depth) => {
                *depth = depth.saturating_sub(1);
                *depth == 0
            }
            None => true,
        };
        if done {
            self.menu_depth.remove(&owner);
        }
        self.finish_menu(owner);
    }

    fn note_menu_window(&mut self, hwnd: HWND) {
        let Some(owner) = self.frosted_owner(hwnd) else {
            return;
        };
        if hwnd.0 as isize == owner {
            return;
        }
        if self.open_menus.insert(hwnd.0 as isize, owner).is_none() {
            self.suspend_owner(owner);
        }
    }

    fn note_menu_window_gone(&mut self, hwnd: HWND) {
        let Some(owner) = self.open_menus.remove(&(hwnd.0 as isize)) else {
            return;
        };
        self.finish_menu(owner);
    }

    fn finish_menu(&mut self, owner: isize) {
        let depth = self.menu_depth.get(&owner).copied().unwrap_or(0);
        let popups = self.open_menus.values().any(|existing| *existing == owner);
        if depth == 0 && !popups {
            self.resume_owner(owner);
        }
    }

    /// A click away closes the menu. Drop holds that are not this window or its menu.
    fn dismiss_menus_unless(&mut self, hwnd: HWND) {
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        let foreground = if root.0.is_null() {
            hwnd.0 as isize
        } else {
            root.0 as isize
        };
        let foreground_owner = self.frosted_owner(hwnd);
        let owners: Vec<isize> = self
            .menu_depth
            .keys()
            .copied()
            .chain(self.open_menus.values().copied())
            .filter(|owner| *owner != foreground && foreground_owner != Some(*owner))
            .collect();
        let mut seen = HashSet::new();
        for owner in owners {
            if !seen.insert(owner) {
                continue;
            }
            self.menu_depth.remove(&owner);
            self.open_menus.retain(|_, held| *held != owner);
            self.resume_owner(owner);
        }
    }

    /// A popup that vanished without a hide event should not leave the app solid.
    fn sweep_menus(&mut self) {
        let stale: Vec<isize> = self
            .open_menus
            .iter()
            .filter(|(token, _)| !target::is_window_showing(target::hwnd_of(**token)))
            .map(|(token, _)| *token)
            .collect();
        for token in stale {
            self.note_menu_window_gone(target::hwnd_of(token));
        }
    }

    fn suspend_owner(&mut self, owner: isize) {
        let Some(tracked) = self.tracked.get_mut(&owner) else {
            return;
        };
        if tracked.menu_suspended {
            return;
        }
        tracked.menu_suspended = true;
        let hwnd = target::hwnd_of(owner);
        let _ = target::set_alpha(hwnd, 255);
        tracked.pane.hide();
    }

    fn resume_owner(&mut self, owner: isize) {
        let Some(tracked) = self.tracked.get_mut(&owner) else {
            return;
        };
        if !tracked.menu_suspended {
            return;
        }
        tracked.menu_suspended = false;
        let hwnd = target::hwnd_of(owner);
        let transparency = tracked.transparency;
        let _ = target::apply_alpha(hwnd, transparency, false);
        tracked.follow(hwnd);
    }

    fn drop_tracked(&mut self, hwnd: isize) {
        self.open_menus.retain(|_, owner| *owner != hwnd);
        self.menu_depth.remove(&hwnd);
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
        self.persist(true);
    }

    /// Record original styles so a crash can be undone on the next start. Writes only on change.
    /// `immediate` is for restore and shutdown. Other updates wait briefly so a burst is one write.
    fn persist(&mut self, immediate: bool) {
        let mut windows: Vec<PersistedWindow> = self
            .tracked
            .iter()
            .map(|(hwnd, tracked)| {
                PersistedWindow::new(
                    *hwnd,
                    tracked.pid,
                    tracked.process_start,
                    tracked.process.clone(),
                    &tracked.saved,
                )
            })
            .chain(self.early.values().map(|(saved, _)| saved.clone()))
            .chain(self.prior.values().cloned())
            .collect();
        windows.sort_by_key(|window| window.hwnd);
        if windows == self.persisted {
            self.pending_state = None;
            self.disarm_persist();
            return;
        }
        if immediate {
            self.write_state(windows);
            return;
        }
        self.pending_state = Some(windows);
        self.arm_persist();
    }

    fn write_state(&mut self, windows: Vec<PersistedWindow>) {
        self.pending_state = None;
        self.disarm_persist();
        if windows != self.persisted && rules::save_state(&windows).is_ok() {
            self.persisted = windows;
        }
    }

    fn arm_persist(&mut self) {
        if self.persist_armed {
            return;
        }
        let hwnd = target::hwnd_of(self.shared.host.load(Ordering::SeqCst));
        if hwnd.0.is_null() {
            if let Some(windows) = self.pending_state.take() {
                if windows != self.persisted && rules::save_state(&windows).is_ok() {
                    self.persisted = windows;
                }
            }
            return;
        }
        self.persist_armed = true;
        unsafe {
            let _ = SetTimer(Some(hwnd), PERSIST_TIMER, PERSIST_MS, None);
        }
    }

    fn disarm_persist(&mut self) {
        if !self.persist_armed {
            return;
        }
        self.persist_armed = false;
        let hwnd = target::hwnd_of(self.shared.host.load(Ordering::SeqCst));
        if !hwnd.0.is_null() {
            unsafe {
                let _ = KillTimer(Some(hwnd), PERSIST_TIMER);
            }
        }
    }

    fn flush_persist(&mut self) {
        self.disarm_persist();
        let Some(windows) = self.pending_state.take() else {
            return;
        };
        if windows != self.persisted && rules::save_state(&windows).is_ok() {
            self.persisted = windows;
        }
    }
}

fn apply_values(
    tracked: &mut Tracked,
    hwnd: HWND,
    transparency: u8,
    blur: u8,
    style: BlurStyle,
    glass: &mut GlassSession,
    shared: &Shared,
) {
    if tracked.transparency != transparency {
        if tracked.menu_suspended {
            tracked.transparency = transparency;
        } else {
            match target::apply_alpha(hwnd, transparency, false) {
                Ok(()) => tracked.transparency = transparency,
                Err(err) => shared.set_status(format!("{}: {err}", tracked.process)),
            }
        }
    }
    if tracked.blur != blur || tracked.style != style {
        match tracked.pane.set_look(glass, style, blur) {
            Ok(()) => {
                tracked.blur = blur;
                tracked.style = style;
            }
            Err(err) => shared.set_status(format!("{}: {err}", tracked.process)),
        }
    }
    shared.fallback.store(glass.fallback(), Ordering::SeqCst);
}

fn install_hooks() -> Vec<HWINEVENTHOOK> {
    let ranges = [
        (EVENT_SYSTEM_MENUPOPUPSTART, EVENT_SYSTEM_MENUPOPUPEND),
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
    if hwnd.0.is_null() {
        return;
    }
    // Menu events name the popup, not a top-level window, so the window filter would drop them.
    let menu = event == EVENT_SYSTEM_MENUPOPUPSTART || event == EVENT_SYSTEM_MENUPOPUPEND;
    if !menu && (id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32) {
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

unsafe extern "system" fn host_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
