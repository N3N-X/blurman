//! The Blurman window.

use crate::autostart;
use crate::ipc;
use crate::mapping::{self, BLUR_DEFAULT, TRANSPARENCY_DEFAULT};
use crate::rules::{self, Rule, Settings, Store};
use crate::shared::Shared;
use crate::target::{self, AppGroup};
use crate::theme::{self, ACCENT, CARD_STROKE, MUTED, ROW_HOVER, ROW_SELECTED, TEXT, WARN};
use crate::tray;
use egui::{Align, Color32, CursorIcon, Frame, Layout, Margin, RichText, Sense, Shape, Slider, Ui};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::GetCurrentThreadId;

const RESCAN: Duration = Duration::from_secs(1);

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
            .with_inner_size([560.0, 800.0])
            .with_min_inner_size([480.0, 600.0])
            .with_icon(icon_rgba()),
        ..Default::default()
    };
    let app_shared = shared.clone();
    let result = eframe::run_native(
        "Blurman",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            app_shared.set_ctx(Some(cc.egui_ctx.clone()));
            if let Ok(handle) = cc.window_handle() {
                if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                    app_shared.main_window.store(win32.hwnd.get(), Ordering::SeqCst);
                }
            }
            Ok(Box::new(BlurmanApp::new(app_shared, settings, tray_session)))
        }),
    )
    .map_err(|err| err.to_string());
    shared.main_window.store(0, Ordering::SeqCst);
    shared.set_ctx(None);
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Apps,
    Settings,
}

struct BlurmanApp {
    shared: Arc<Shared>,
    store: Store,
    settings: Settings,
    autostart: bool,
    tray_error: Option<String>,
    tab: Tab,
    seen_generation: u64,
    groups: Vec<AppGroup>,
    scanned: Instant,
    selected: Option<String>,
    transparency: u8,
    blur: u8,
    solid_text: bool,
    /// Launched by Windows startup: this session lives in the tray whatever the setting says.
    startup: bool,
}

impl BlurmanApp {
    fn new(shared: Arc<Shared>, settings: Settings, startup: bool) -> Self {
        let store = rules::load();
        let tray_error = tray::sync(settings.close_to_tray || startup, store.paused).err();
        Self {
            shared,
            store,
            settings,
            autostart: autostart::is_enabled(),
            tray_error,
            tab: Tab::Apps,
            seen_generation: 0,
            groups: target::list_groups(),
            scanned: Instant::now(),
            selected: None,
            transparency: TRANSPARENCY_DEFAULT,
            blur: BLUR_DEFAULT,
            solid_text: false,
            startup,
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
            (self.transparency, self.blur, self.solid_text) = (rule.transparency, rule.blur, rule.solid_text);
        }
    }

    fn selected_rule(&self) -> Option<&Rule> {
        self.selected.as_deref().and_then(|process| self.store.rule(process))
    }

    fn publish(&mut self) {
        self.store.normalize();
        if let Err(err) = rules::save(&self.store) {
            self.shared.set_status(format!("Could not save rules: {err}"));
            return;
        }
        ipc::signal(ipc::msg_reload());
    }

    fn frost_selected(&mut self) {
        let Some(process) = self.selected.clone() else {
            return;
        };
        self.store.paused = false;
        self.store
            .upsert(rules::new_rule(&process, self.transparency, self.blur, self.solid_text));
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

    fn header(&mut self, ui: &mut Ui) {
        let roomy = ui.available_width() >= 500.0;
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), Sense::hover());
            let painter = ui.painter();
            painter.circle_filled(rect.center(), 16.0, Color32::from_rgb(58, 96, 132));
            painter.circle_filled(rect.center(), 12.0, Color32::from_rgb(176, 214, 232));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.heading(RichText::new("Blurman").strong().color(TEXT));
                if roomy {
                    ui.label(theme::muted("Frosted glass behind the apps you pick").small());
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                Frame::new()
                    .fill(theme::CARD)
                    .stroke(theme::line(1.0, CARD_STROKE))
                    .corner_radius(10)
                    .inner_margin(Margin::same(3))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.selectable_value(&mut self.tab, Tab::Settings, " Settings ");
                        ui.selectable_value(&mut self.tab, Tab::Apps, " Apps ");
                    });
            });
        });
    }

    fn banners(&mut self, ui: &mut Ui) {
        if self.store.paused {
            banner(ui, WARN, |ui| {
                ui.label(RichText::new("Paused. Every app is back to normal.").color(WARN));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(theme::primary_button("Resume").min_size(egui::vec2(80.0, 26.0))).clicked() {
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

    fn apps_tab(&mut self, ui: &mut Ui) {
        self.running_apps(ui);
        ui.add_space(4.0);
        self.glass_controls(ui);
        ui.add_space(4.0);
        self.saved_rules(ui);
    }

    fn running_apps(&mut self, ui: &mut Ui) {
        let mut picked = None;
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::card_title("Running apps"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(theme::quiet_button("Refresh").small()).clicked() {
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
                        let frosted = self.store.rule(&group.process).is_some_and(|rule| rule.enabled);
                        if app_row(ui, group, selected, frosted).clicked() {
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
                ui.label(theme::muted("Pick an app above, set the look, then frost it."));
            }
            ui.add_space(4.0);
            ui.spacing_mut().slider_width = (ui.available_width() - 190.0).clamp(100.0, 320.0);
            let mut moved = false;
            egui::Grid::new("glass")
                .num_columns(2)
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Transparency");
                    moved |= ui
                        .add(
                            Slider::new(
                                &mut self.transparency,
                                mapping::TRANSPARENCY_MIN..=mapping::TRANSPARENCY_MAX,
                            )
                            .suffix("%"),
                        )
                        .changed();
                    ui.end_row();
                    ui.label("Blur");
                    moved |= ui
                        .add(Slider::new(&mut self.blur, mapping::BLUR_MIN..=mapping::BLUR_MAX))
                        .changed();
                    ui.end_row();
                });
            ui.add_space(6.0);
            moved |= setting_row(
                ui,
                "Solid text  ·  experimental",
                "Only the background turns to glass; text and images stay solid. Blurman shows a \
                 live copy of the app, so it uses a little GPU and lags about one frame.",
                &mut self.solid_text,
            );
            if ruled && moved {
                self.frost_selected();
            }
            ui.add_space(6.0);
            ui.add_enabled_ui(self.selected.is_some(), |ui| {
                if ruled {
                    ui.horizontal(|ui| {
                        if ui.add(theme::danger_button("Remove frost").min_size(egui::vec2(120.0, 30.0))).clicked() {
                            self.unfrost_selected();
                        }
                        ui.label(theme::muted("Changes apply as you drag.").small());
                    });
                } else if ui.add(theme::primary_button("Frost it")).clicked() {
                    self.frost_selected();
                }
            });
        });
    }

    fn saved_rules(&mut self, ui: &mut Ui) {
        let mut changed = false;
        let mut delete = None;
        let mut pause = None;
        let mut restore_all = false;
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::card_title("Saved rules"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let any = !self.store.rules.is_empty();
                    if ui
                        .add_enabled(any, theme::danger_button("Restore all").small())
                        .on_hover_text("Delete every rule and put every app back.")
                        .clicked()
                    {
                        restore_all = true;
                    }
                    let label = if self.store.paused { "Resume" } else { "Pause" };
                    if ui
                        .add_enabled(any, theme::quiet_button(label).small())
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
                    .fill(ROW_HOVER)
                    .corner_radius(10)
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            changed |= theme::toggle(ui, &mut rule.enabled)
                                .on_hover_text("Turn this rule on or off.")
                                .changed();
                            let color = if rule.enabled { TEXT } else { MUTED };
                            ui.label(RichText::new(&rule.process).strong().color(color));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.add(theme::danger_button("Remove").small()).clicked() {
                                    delete = Some(rule.process.clone());
                                }
                                if rule.solid_text {
                                    theme::badge(ui, "Solid text", ACCENT);
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            // Room left after both labels and both value boxes.
                            ui.spacing_mut().slider_width =
                                ((ui.available_width() - 290.0) / 2.0).clamp(60.0, 180.0);
                            ui.label(theme::muted("Transparency").small());
                            changed |= ui
                                .add(
                                    Slider::new(
                                        &mut rule.transparency,
                                        mapping::TRANSPARENCY_MIN..=mapping::TRANSPARENCY_MAX,
                                    )
                                    .suffix("%"),
                                )
                                .changed();
                            ui.add_space(6.0);
                            ui.label(theme::muted("Blur").small());
                            changed |= ui
                                .add(Slider::new(&mut rule.blur, mapping::BLUR_MIN..=mapping::BLUR_MAX))
                                .changed();
                        });
                    });
            }
        });
        if let Some(process) = delete {
            self.store.remove(&process);
            changed = true;
        }
        if restore_all {
            self.store.clear();
            changed = true;
        }
        if let Some(paused) = pause {
            self.store.paused = paused;
            changed = true;
        }
        if changed {
            self.sliders_from_rule();
            self.publish();
        }
    }

    fn settings_tab(&mut self, ui: &mut Ui) {
        theme::card(ui, |ui| {
            ui.label(theme::card_title("Startup and closing"));
            ui.add_space(4.0);
            let mut autostart = self.autostart;
            if setting_row(
                ui,
                "Start with Windows",
                "Open Blurman when you sign in, so your apps are frosted right away. \
                 It starts quietly in the tray; click the tray icon to open this window.",
                &mut autostart,
            ) {
                match autostart::set_enabled(autostart) {
                    Ok(()) => self.autostart = autostart,
                    Err(err) => self.shared.set_status(err),
                }
            }
            ui.separator();
            if setting_row(
                ui,
                "Keep in tray on close",
                "Closing the window hides Blurman in the tray and keeps your apps frosted. \
                 Choose Exit from the tray menu to put them back.",
                &mut self.settings.close_to_tray,
            ) {
                if let Err(err) = rules::save_settings(&self.settings) {
                    self.shared.set_status(format!("Could not save settings: {err}"));
                }
                self.tray_error = tray::sync(self.wants_tray(), self.store.paused).err();
            }
            if let Some(err) = &self.tray_error {
                ui.colored_label(WARN, err);
            }
        });
        ui.add_space(4.0);
        theme::card(ui, |ui| {
            ui.label(theme::card_title("Solid text"));
            if setting_row(
                ui,
                "Keep solid text during fullscreen games",
                "Off: while a game or other fullscreen app is in front, solid-text apps switch to \
                 the normal fade so capturing them never costs the game anything.",
                &mut self.settings.solid_text_in_fullscreen,
            ) {
                if let Err(err) = rules::save_settings(&self.settings) {
                    self.shared.set_status(format!("Could not save settings: {err}"));
                }
                ipc::signal(ipc::msg_reload());
            }
        });
        ui.add_space(4.0);
        theme::card(ui, |ui| {
            ui.label(theme::card_title("Files"));
            ui.label(theme::muted("Rules and settings are saved in"));
            let dir = rules::app_dir();
            ui.label(RichText::new(dir.display().to_string()).monospace().color(TEXT));
            if ui.add(theme::quiet_button("Open folder")).clicked() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
        });
        ui.add_space(4.0);
        theme::card(ui, |ui| {
            ui.label(theme::card_title("About"));
            ui.label(theme::muted(format!(
                "Blurman {}. The app is faded and a blurred glass window sits right behind it, \
                 so whatever shows through is frosted rather than sharp.",
                env!("CARGO_PKG_VERSION")
            )));
        });
    }
}

impl eframe::App for BlurmanApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(RESCAN);
        if ctx.input(|input| input.viewport().close_requested()) && self.keeps_in_tray() {
            self.shared.to_tray.store(true, Ordering::SeqCst);
        }
        self.sync_from_disk();
        if self.scanned.elapsed() >= RESCAN {
            self.rescan();
        }
        if self.wants_tray() {
            self.tray_error = tray::sync(true, self.store.paused).err();
        }

        let panel = |margin: Margin| Frame::new().fill(theme::BG).inner_margin(margin);
        egui::TopBottomPanel::top("header")
            .frame(panel(Margin { left: 18, right: 18, top: 14, bottom: 10 }))
            .show_separator_line(false)
            .show(ctx, |ui| self.header(ui));
        let footer = if self.keeps_in_tray() {
            "Closing hides Blurman in the tray. Exit from the tray puts every app back."
        } else {
            "Closing Blurman puts every app back to normal."
        };
        egui::TopBottomPanel::bottom("footer")
            .frame(panel(Margin::symmetric(18, 8)))
            .show_separator_line(false)
            .show(ctx, |ui| ui.label(theme::muted(footer).small()));
        egui::CentralPanel::default()
            .frame(panel(Margin::symmetric(18, 4)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.banners(ui);
                        match self.tab {
                            Tab::Apps => self.apps_tab(ui),
                            Tab::Settings => self.settings_tab(ui),
                        }
                        ui.add_space(8.0);
                    });
            });
    }
}

fn banner(ui: &mut Ui, color: Color32, add_contents: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(theme::line(1.0, color.gamma_multiply(0.35)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(add_contents);
        });
    ui.add_space(4.0);
}

fn app_row(ui: &mut Ui, group: &AppGroup, selected: bool, frosted: bool) -> egui::Response {
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
                    if frosted {
                        theme::badge(ui, "Frosted", ACCENT);
                    }
                    if group.elevated {
                        theme::badge(ui, "Admin", WARN);
                    }
                });
            });
        });
    let rect = inner.response.rect;
    let response = ui
        .interact(rect, ui.id().with(("app", &group.process)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    let fill = if selected {
        ROW_SELECTED
    } else if response.hovered() {
        ROW_HOVER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().set(background, Shape::rect_filled(rect, 8, fill));
    response
}

/// A title, a description, and a switch on the right. Returns true when the switch was flipped.
fn setting_row(ui: &mut Ui, title: &str, description: &str, on: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let text_width = (ui.available_width() - 60.0).max(160.0);
        ui.vertical(|ui| {
            ui.set_width(text_width);
            ui.label(RichText::new(title).strong().color(TEXT));
            ui.add(egui::Label::new(theme::muted(description).small()).wrap());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            changed = theme::toggle(ui, on).changed();
        });
    });
    changed
}

pub fn icon_rgba() -> egui::IconData {
    let size = 32i32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x - 16, y - 16);
            let distance = dx * dx + dy * dy;
            let pixel = if distance <= 11 * 11 {
                [176, 214, 232, 255]
            } else if distance <= 15 * 15 {
                [58, 96, 132, 255]
            } else {
                continue;
            };
            let index = ((y * size + x) * 4) as usize;
            rgba[index..index + 4].copy_from_slice(&pixel);
        }
    }
    egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}
