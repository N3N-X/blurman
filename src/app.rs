//! The Blurman window.

use crate::autostart;
use crate::ipc;
use crate::mapping::{self, BlurStyle, BLUR_DEFAULT, TRANSPARENCY_DEFAULT};
use crate::rules::{self, Rule, Settings, Store};
use crate::shared::{Shared, Tweak};
use crate::target::{self, AppGroup};
use crate::theme::{self, ACCENT, MUTED, OK, ROW_HOVER, ROW_SELECTED, TEXT, WARN};
use crate::tray;
use egui::{
    Align, Color32, CursorIcon, Frame, Layout, Margin, RichText, Sense, Shape, Slider, Stroke, Ui,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_TRANSIENTWINDOW,
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_TEXT_COLOR,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::MARGINS;

const RESCAN: Duration = Duration::from_secs(1);
/// How long a slider may sit still, while the button is held, before the rules file is written.
const SAVE_AFTER: Duration = Duration::from_millis(80);

/// Show the window, and while Blurman sits in the tray, close it for real. eframe spins a CPU
/// core when its window is merely hidden, so the tray waits on plain Win32 messages instead.
pub fn run(shared: Arc<Shared>, startup: bool) -> Result<(), String> {
    shared
        .ui_thread
        .store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    tray::install_handlers(shared.clone());
    // A startup launch lives in the tray, and closing its window goes back there.
    let mut in_tray = startup && tray::sync(true, rules::load().paused).is_ok();
    loop {
        if in_tray && !tray::wait_for_open() {
            return Ok(());
        }
        open_window(&shared, startup)?;
        if !shared.to_tray.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        in_tray = true;
    }
}

fn open_window(shared: &Arc<Shared>, tray_session: bool) -> Result<(), String> {
    let settings = rules::load_settings();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Blurman")
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([900.0, 560.0])
            .with_transparent(true)
            .with_icon(window_icon()),
        ..Default::default()
    };
    let app_shared = shared.clone();
    let result = eframe::run_native(
        "Blurman",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            app_shared.set_ctx(Some(cc.egui_ctx.clone()));
            let mut glass = false;
            if let Ok(handle) = cc.window_handle() {
                if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                    app_shared
                        .main_window
                        .store(win32.hwnd.get(), Ordering::SeqCst);
                    glass = frost_window(HWND(win32.hwnd.get() as *mut _));
                }
            }
            Ok(Box::new(BlurmanApp::new(
                app_shared,
                settings,
                tray_session,
                glass,
                &cc.egui_ctx,
            )))
        }),
    )
    .map_err(|err| err.to_string());
    shared.main_window.store(0, Ordering::SeqCst);
    shared.set_ctx(None);
    result
}

/// Give the window the Windows 11 acrylic backdrop, so Blurman itself is frosted glass.
/// False on Windows versions without system backdrops, where the window stays opaque.
fn frost_window(hwnd: HWND) -> bool {
    let set_i32 = |attribute: DWMWINDOWATTRIBUTE, value: i32| unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute,
            (&raw const value).cast(),
            size_of::<i32>() as u32,
        )
    };
    let set_color = |attribute: DWMWINDOWATTRIBUTE, value: u32| unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute,
            (&raw const value).cast(),
            size_of::<u32>() as u32,
        )
    };
    let _ = set_i32(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    // Same caption treatment as the pump screen window: round corners, plum border, light text.
    let _ = set_i32(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0);
    let _ = set_color(DWMWA_BORDER_COLOR, 0x0030_2832);
    let _ = set_color(DWMWA_CAPTION_COLOR, 0x0018_1016);
    let _ = set_color(DWMWA_TEXT_COLOR, 0x00F2_EEF0);
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) }.is_ok()
        && set_i32(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_TRANSIENTWINDOW.0).is_ok()
}

struct BlurmanApp {
    shared: Arc<Shared>,
    store: Store,
    settings: Settings,
    autostart: bool,
    tray_error: Option<String>,
    seen_generation: u64,
    groups: Vec<AppGroup>,
    scanned: Instant,
    selected: Option<String>,
    transparency: u8,
    blur: u8,
    style: BlurStyle,
    /// Launched by Windows startup: this session lives in the tray whatever the setting says.
    startup: bool,
    /// The window has a frosted backdrop showing through wherever nothing is drawn.
    glass: bool,
    /// A window saved from the old narrow layout is widened once.
    window_fitted: bool,
    /// When a slider edit should be written. None while the file matches the sliders.
    pending_save: Option<Instant>,
}

impl BlurmanApp {
    fn new(
        shared: Arc<Shared>,
        settings: Settings,
        startup: bool,
        glass: bool,
        _ctx: &egui::Context,
    ) -> Self {
        let store = rules::load();
        let tray_error = tray::sync(settings.close_to_tray || startup, store.paused).err();
        Self {
            shared,
            store,
            settings,
            autostart: autostart::is_enabled(),
            tray_error,
            seen_generation: 0,
            groups: target::list_groups(),
            scanned: Instant::now(),
            selected: None,
            transparency: TRANSPARENCY_DEFAULT,
            blur: BLUR_DEFAULT,
            style: BlurStyle::Frost,
            startup,
            glass,
            window_fitted: false,
            pending_save: None,
        }
    }

    fn wants_tray(&self) -> bool {
        self.settings.close_to_tray || self.startup
    }

    fn keeps_in_tray(&self) -> bool {
        self.wants_tray() && self.tray_error.is_none()
    }

    fn rescan(&mut self) {
        self.groups = target::list_groups();
        self.scanned = Instant::now();
    }

    /// Pick up rule changes made from the tray or the command line.
    fn sync_from_disk(&mut self) {
        let generation = self.shared.rules_generation.load(Ordering::SeqCst);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            self.store = rules::load();
            self.sliders_from_rule();
        }
    }

    fn sliders_from_rule(&mut self) {
        if let Some(rule) = self.selected_rule() {
            (self.transparency, self.blur, self.style) = (rule.transparency, rule.blur, rule.style);
        }
    }

    fn selected_rule(&self) -> Option<&Rule> {
        self.selected
            .as_deref()
            .and_then(|process| self.store.rule(process))
    }

    fn publish(&mut self) {
        self.pending_save = None;
        self.store.normalize();
        if let Err(err) = rules::save(&self.store) {
            self.shared
                .set_status(format!("Could not save rules: {err}"));
            return;
        }
        ipc::signal(ipc::msg_reload());
    }

    /// Push a slider change to the panes now. The file is written once the drag settles.
    fn live_rule(&mut self, process: &str) {
        let Some(rule) = self.store.rule(process) else {
            return;
        };
        self.shared.push_tweak(Tweak {
            process: rule.process.clone(),
            transparency: rule.transparency,
            blur: rule.blur,
            style: rule.style,
        });
        ipc::signal(ipc::msg_tweak());
        self.pending_save = Some(Instant::now() + SAVE_AFTER);
    }

    fn flush_rules_file(&mut self) {
        self.pending_save = None;
        self.store.normalize();
        if let Err(err) = rules::save(&self.store) {
            self.shared
                .set_status(format!("Could not save rules: {err}"));
        }
    }

    /// Write the rules file once a drag ends, or about 80 ms after the last movement.
    fn settle_save(&mut self, pointer_down: bool) -> Duration {
        let Some(deadline) = self.pending_save else {
            return RESCAN;
        };
        if pointer_down && deadline > Instant::now() {
            return deadline
                .saturating_duration_since(Instant::now())
                .min(RESCAN);
        }
        self.flush_rules_file();
        RESCAN
    }

    fn frost_selected(&mut self) {
        let Some(process) = self.selected.clone() else {
            return;
        };
        self.store.paused = false;
        self.store.upsert(rules::new_rule(
            &process,
            self.transparency,
            self.blur,
            self.style,
        ));
        self.publish();
    }

    fn unfrost_selected(&mut self) {
        if let Some(process) = self.selected.clone() {
            self.store.remove(&process);
            self.publish();
        }
    }

    fn set_paused(&mut self, paused: bool) {
        self.store.paused = paused;
        self.publish();
    }

    fn fit_window(&mut self, ctx: &egui::Context) {
        if self.window_fitted {
            return;
        }
        self.window_fitted = true;
        let narrow = ctx.input(|input| {
            input
                .viewport()
                .inner_rect
                .map(|rect| rect.width() < 900.0)
                .unwrap_or(true)
        });
        if narrow {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1000.0, 680.0)));
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 22.0), Sense::hover());
            let mark = if self.store.paused { WARN } else { ACCENT };
            ui.painter().circle_filled(rect.center(), 5.0, mark);
            ui.heading(RichText::new("Blurman").color(TEXT));
            ui.label(theme::muted("Frosted glass behind the apps you pick"));
            let frosted = self.store.rules.iter().filter(|rule| rule.enabled).count();
            let (text, color) = if self.store.paused {
                ("Paused".to_string(), WARN)
            } else if frosted == 0 {
                ("Ready".to_string(), MUTED)
            } else if frosted == 1 {
                ("1 frosted".to_string(), OK)
            } else {
                (format!("{frosted} frosted"), OK)
            };
            theme::status_pill(ui, &text, color);
        });
    }

    fn banners(&mut self, ui: &mut Ui) {
        if self.store.paused {
            banner(ui, WARN, |ui| {
                ui.label(RichText::new("Paused. Every app is back to normal.").color(WARN));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(theme::primary_button("Resume").min_size(egui::vec2(80.0, 26.0)))
                        .clicked()
                    {
                        self.set_paused(false);
                    }
                });
            });
        }
        let status = self.shared.status();
        if !status.is_empty() {
            banner(ui, WARN, |ui| {
                ui.add(egui::Label::new(RichText::new(status).color(WARN)).wrap());
            });
        }
        if self.shared.fallback.load(Ordering::SeqCst) {
            banner(ui, MUTED, |ui| {
                ui.add(
                    egui::Label::new(theme::muted(
                        "This PC uses system acrylic, so the blur slider sets how milky the glass is.",
                    ))
                    .wrap(),
                );
            });
        }
    }

    fn running_apps(&mut self, ui: &mut Ui) {
        let mut picked = None;
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::card_title("Running apps"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.rescan();
                    }
                });
            });
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .id_salt("running")
                .max_height(250.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if self.groups.is_empty() {
                        ui.label(theme::muted("No normal windows are open."));
                    }
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for group in &self.groups {
                        let selected = self.selected.as_deref() == Some(group.process.as_str());
                        let rule = self.store.rule(&group.process).filter(|rule| rule.enabled);
                        if app_row(ui, group, selected, rule).clicked() {
                            picked = Some(group.process.clone());
                        }
                    }
                });
        });
        if let Some(process) = picked {
            self.selected = Some(process);
            self.sliders_from_rule();
        }
    }

    fn glass_controls(&mut self, ui: &mut Ui) {
        theme::card(ui, |ui| {
            let ruled = self.selected_rule().is_some();
            ui.horizontal(|ui| {
                ui.label(theme::card_title("Glass"));
                if let Some(process) = &self.selected {
                    ui.label(theme::muted("for"));
                    ui.label(RichText::new(process).strong().color(ACCENT));
                }
            });
            if self.selected.is_none() {
                ui.label(theme::muted(
                    "Pick an app on the left, set the look, then frost it.",
                ));
            }
            ui.add_space(4.0);
            let mut moved = false;
            ui.horizontal(|ui| {
                ui.label(theme::muted("Look"));
                moved |= look_picker(ui, &mut self.style);
            });
            let milky =
                self.style == BlurStyle::Acrylic || self.shared.fallback.load(Ordering::SeqCst);
            moved |= labeled_slider(ui, "Transparency", |ui| {
                ui.add(
                    Slider::new(
                        &mut self.transparency,
                        mapping::TRANSPARENCY_MIN..=mapping::TRANSPARENCY_MAX,
                    )
                    .suffix("%"),
                )
            });
            moved |= labeled_slider(ui, if milky { "Milkiness" } else { "Blur" }, |ui| {
                ui.add(Slider::new(
                    &mut self.blur,
                    mapping::BLUR_MIN..=mapping::BLUR_MAX,
                ))
            });
            if !self.shared.fallback.load(Ordering::SeqCst) {
                ui.label(
                    theme::muted(match self.style {
                        BlurStyle::Frost => "Blurs whatever is behind the window.",
                        BlurStyle::Acrylic => {
                            "Milky system glass. The slider sets how milky it is."
                        }
                    })
                    .small(),
                );
            }
            if ruled && moved {
                let was_enabled = self.selected_rule().is_some_and(|rule| rule.enabled);
                if self.store.paused || !was_enabled {
                    // Enabling, or waking from pause, has to attach panes. A pure slider edit does not.
                    self.frost_selected();
                } else if let Some(process) = self.selected.clone() {
                    self.store.upsert(rules::new_rule(
                        &process,
                        self.transparency,
                        self.blur,
                        self.style,
                    ));
                    self.live_rule(&process);
                }
            }
            ui.add_space(6.0);
            ui.add_enabled_ui(self.selected.is_some(), |ui| {
                ui.horizontal(|ui| {
                    if ruled {
                        if ui.button("Remove frost").clicked() {
                            self.unfrost_selected();
                        }
                        ui.label(theme::muted("Changes apply as you drag."));
                    } else if ui.add(theme::primary_button("Frost it")).clicked() {
                        self.frost_selected();
                    }
                });
            });
        });
    }

    fn saved_rules(&mut self, ui: &mut Ui) {
        let mut structural = false;
        let mut tweaked = Vec::new();
        let mut delete = None;
        let mut pause = None;
        let mut restore_all = false;
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::card_title("Saved rules"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let any = !self.store.rules.is_empty();
                    if ui
                        .add_enabled(any, egui::Button::new("Restore all"))
                        .on_hover_text("Delete every rule and put every app back.")
                        .clicked()
                    {
                        restore_all = true;
                    }
                    let label = if self.store.paused { "Resume" } else { "Pause" };
                    if ui
                        .add_enabled(any, egui::Button::new(label))
                        .on_hover_text("Put every app back without deleting rules.")
                        .clicked()
                    {
                        pause = Some(!self.store.paused);
                    }
                });
            });
            if self.store.rules.is_empty() {
                ui.label(theme::muted("Nothing is frosted yet."));
                return;
            }
            ui.add_space(2.0);
            for rule in &mut self.store.rules {
                Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 10))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 28),
                    ))
                    .corner_radius(12)
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let color = if rule.enabled { TEXT } else { MUTED };
                            structural |= ui
                                .checkbox(
                                    &mut rule.enabled,
                                    RichText::new(&rule.process).strong().color(color),
                                )
                                .on_hover_text("Turn this rule on or off.")
                                .changed();
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Remove").clicked() {
                                    delete = Some(rule.process.clone());
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label(theme::muted("Look"));
                            let look = look_picker(ui, &mut rule.style);
                            if look && !tweaked.iter().any(|item: &String| item == &rule.process) {
                                tweaked.push(rule.process.clone());
                            }
                        });
                        ui.horizontal(|ui| {
                            // Room left after both labels and both value boxes.
                            let milky = rule.style == BlurStyle::Acrylic;
                            ui.spacing_mut().slider_width =
                                ((ui.available_width() - if milky { 330.0 } else { 290.0 }) / 2.0)
                                    .clamp(60.0, 180.0);
                            ui.label(theme::muted("Transparency").small());
                            let transparency = ui.add(
                                Slider::new(
                                    &mut rule.transparency,
                                    mapping::TRANSPARENCY_MIN..=mapping::TRANSPARENCY_MAX,
                                )
                                .suffix("%"),
                            );
                            ui.add_space(6.0);
                            ui.label(theme::muted(if milky { "Milky" } else { "Blur" }).small());
                            let blur = ui.add(Slider::new(
                                &mut rule.blur,
                                mapping::BLUR_MIN..=mapping::BLUR_MAX,
                            ));
                            if (transparency.changed() || blur.changed())
                                && !tweaked.iter().any(|item: &String| item == &rule.process)
                            {
                                tweaked.push(rule.process.clone());
                            }
                        });
                    });
            }
        });
        if let Some(process) = delete {
            self.store.remove(&process);
            structural = true;
        }
        if restore_all {
            self.store.clear();
            structural = true;
        }
        if let Some(paused) = pause {
            self.store.paused = paused;
            structural = true;
        }
        if structural {
            self.sliders_from_rule();
            self.publish();
        } else if !tweaked.is_empty() {
            self.sliders_from_rule();
            for process in tweaked {
                self.live_rule(&process);
            }
        }
    }

    fn startup_card(&mut self, ui: &mut Ui) {
        theme::card(ui, |ui| {
            ui.label(theme::card_title("Startup and tray"));
            ui.add_space(4.0);
            let mut autostart = self.autostart;
            if ui
                .checkbox(&mut autostart, "Start when I sign in to Windows")
                .changed()
            {
                match autostart::set_enabled(autostart) {
                    Ok(()) => self.autostart = autostart,
                    Err(err) => self.shared.set_status(err),
                }
            }
            if ui
                .checkbox(
                    &mut self.settings.close_to_tray,
                    "Keep running in the tray when the window closes",
                )
                .changed()
            {
                if let Err(err) = rules::save_settings(&self.settings) {
                    self.shared
                        .set_status(format!("Could not save settings: {err}"));
                }
                self.tray_error = tray::sync(self.wants_tray(), self.store.paused).err();
            }
            if let Some(err) = &self.tray_error {
                ui.colored_label(WARN, err);
            }
            ui.label(theme::muted(
                "Click the tray icon to show this window. Right-click it to pause or exit.",
            ));
        });
    }

    fn files_card(&mut self, ui: &mut Ui) {
        theme::card(ui, |ui| {
            ui.label(theme::card_title("Files"));
            ui.label(theme::muted("Rules and settings are saved in"));
            let dir = rules::app_dir();
            ui.label(
                RichText::new(dir.display().to_string())
                    .monospace()
                    .color(TEXT),
            );
            if ui.button("Open folder").clicked() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
            ui.label(
                theme::muted(format!(
                    "Blurman {}. The app is faded, and a blurred glass window sits right behind it.",
                    env!("CARGO_PKG_VERSION")
                ))
                .small(),
            );
        });
    }
}

impl eframe::App for BlurmanApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.glass {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            theme::BG.to_normalized_gamma_f32()
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|input| input.viewport().close_requested()) {
            // A drag can end by closing the window, before the settle timer writes the file.
            if self.pending_save.is_some() {
                self.flush_rules_file();
            }
            if self.keeps_in_tray() {
                self.shared.to_tray.store(true, Ordering::SeqCst);
            }
        }
        self.sync_from_disk();
        if self.scanned.elapsed() >= RESCAN {
            self.rescan();
        }
        if self.wants_tray() {
            self.tray_error = tray::sync(true, self.store.paused).err();
        }

        self.fit_window(ctx);
        let footer = if self.keeps_in_tray() {
            "Closing hides Blurman in the tray. Exit from the tray puts every app back."
        } else {
            "Closing Blurman puts every app back to normal."
        };
        egui::CentralPanel::default()
            .frame(theme::shell(self.glass))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.header(ui);
                        ui.add_space(8.0);
                        self.banners(ui);
                        ui.columns_const(|[ref mut left, ref mut right]| {
                            self.running_apps(left);
                            right.vertical(|ui| {
                                self.glass_controls(ui);
                                ui.add_space(8.0);
                                self.startup_card(ui);
                                ui.add_space(8.0);
                                self.files_card(ui);
                            });
                        });
                        ui.add_space(8.0);
                        self.saved_rules(ui);
                        ui.add_space(8.0);
                        ui.label(RichText::new(footer).size(13.5).color(MUTED));
                    });
            });
        let pointer_down = ctx.input(|input| input.pointer.any_down());
        ctx.request_repaint_after(self.settle_save(pointer_down));
    }
}

fn look_picker(ui: &mut Ui, style: &mut BlurStyle) -> bool {
    let mut changed = false;
    if theme::choice(ui, *style == BlurStyle::Frost, "Frost")
        .on_hover_text("Blur whatever is behind the window. The slider sets how far it spreads.")
        .clicked()
        && *style != BlurStyle::Frost
    {
        *style = BlurStyle::Frost;
        changed = true;
    }
    if theme::choice(ui, *style == BlurStyle::Acrylic, "Acrylic")
        .on_hover_text("Milky system glass. The slider sets how milky it is.")
        .clicked()
        && *style != BlurStyle::Acrylic
    {
        *style = BlurStyle::Acrylic;
        changed = true;
    }
    changed
}

fn labeled_slider(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> egui::Response) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(theme::muted(label));
        ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(80.0);
        changed = add(ui).changed();
    });
    changed
}

fn banner(ui: &mut Ui, color: Color32, add_contents: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(color.gamma_multiply(0.22))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.5)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(add_contents);
        });
    ui.add_space(8.0);
}

fn app_row(ui: &mut Ui, group: &AppGroup, selected: bool, rule: Option<&Rule>) -> egui::Response {
    let background = ui.painter().add(Shape::Noop);
    let inner = Frame::new()
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let text_width = (ui.available_width() - 160.0).max(140.0);
                ui.vertical(|ui| {
                    ui.set_max_width(text_width);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new(&group.process).strong().color(TEXT));
                    ui.add(egui::Label::new(theme::muted(&group.sample_title).small()).truncate());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let count = format!(
                        "{} window{}",
                        group.windows,
                        if group.windows == 1 { "" } else { "s" }
                    );
                    ui.label(theme::muted(count).small());
                    if let Some(rule) = rule {
                        let look = match rule.style {
                            BlurStyle::Frost => "Frost",
                            BlurStyle::Acrylic => "Acrylic",
                        };
                        theme::status_pill(ui, &format!("{look} · {}%", rule.transparency), ACCENT);
                    }
                    if group.fullscreen {
                        theme::status_pill(ui, "Fullscreen", WARN);
                    }
                    if group.elevated {
                        theme::status_pill(ui, "Admin", WARN);
                    }
                });
            });
        });
    let rect = inner.response.rect;
    let mut response = ui
        .interact(rect, ui.id().with(("app", &group.process)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if group.fullscreen {
        response =
            response.on_hover_text("This app covers the screen, so Blurman leaves it alone.");
    }
    let fill = if selected {
        ROW_SELECTED
    } else if response.hovered() {
        ROW_HOVER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .set(background, Shape::rect_filled(rect, 8, fill));
    response
}

fn decode_icon(bytes: &[u8]) -> egui::IconData {
    let image = image::load_from_memory(bytes).expect("app icon");
    let rgba = image.to_rgba8();
    egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

pub fn window_icon() -> egui::IconData {
    decode_icon(include_bytes!("../assets/icon-256.png"))
}

pub fn tray_icon() -> egui::IconData {
    decode_icon(include_bytes!("../assets/icon-32.png"))
}
