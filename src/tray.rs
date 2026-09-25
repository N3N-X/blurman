//! The tray icon, present while "Keep in tray on close" is on. Lives on the UI thread.

use crate::ipc;
use crate::rules;
use crate::shared::Shared;
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const OPEN: &str = "open";
const PAUSE: &str = "pause";
const EXIT: &str = "exit";

struct Tray {
    _icon: TrayIcon,
    pause: CheckMenuItem,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Menu and click events arrive on the UI thread even while the window is hidden and egui
/// is not running frames, so they are handled here instead of in `update`.
pub fn install_handlers(shared: Arc<Shared>) {
    let menu_shared = shared.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
        OPEN => menu_shared.show_window(),
        PAUSE => toggle_pause(&menu_shared),
        EXIT => exit(&menu_shared),
        _ => {}
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            shared.show_window();
        }
    }));
}

/// Create or remove the icon to match the setting, and keep the Pause check in step.
pub fn sync(enabled: bool, paused: bool) -> Result<(), String> {
    TRAY.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return Ok(());
        };
        if !enabled {
            *slot = None;
            return Ok(());
        }
        if slot.is_none() {
            *slot = Some(build(paused)?);
        }
        if let Some(tray) = slot.as_ref() {
            if tray.pause.is_checked() != paused {
                tray.pause.set_checked(paused);
            }
        }
        Ok(())
    })
}

fn build(paused: bool) -> Result<Tray, String> {
    let open = MenuItem::with_id(OPEN, "Open Blurman", true, None);
    let pause = CheckMenuItem::with_id(PAUSE, "Pause", true, paused, None);
    let exit = MenuItem::with_id(EXIT, "Exit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &pause, &PredefinedMenuItem::separator(), &exit])
        .map_err(|err| err.to_string())?;
    let icon = crate::app::icon_rgba();
    let icon = tray_icon::Icon::from_rgba(icon.rgba, icon.width, icon.height)
        .map_err(|err| err.to_string())?;
    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Blurman")
        .with_icon(icon)
        .build()
        .map_err(|err| format!("Could not add the tray icon: {err}"))?;
    Ok(Tray { _icon: icon, pause })
}

fn toggle_pause(shared: &Shared) {
    let mut store = rules::load();
    store.paused = !store.paused;
    if let Err(err) = rules::save(&store) {
        shared.set_status(format!("Could not save rules: {err}"));
        return;
    }
    ipc::signal(ipc::msg_reload());
    let _ = sync(true, store.paused);
}

fn exit(shared: &Shared) {
    shared.shutdown_worker(Duration::from_secs(3));
    TRAY.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            slot.take();
        }
    });
    std::process::exit(0);
}
