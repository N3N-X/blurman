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
    /// The 150% display was painting into a window sized as if it were 100%.
    dpi_matched: bool,
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
            dpi_matched: false,
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

    /// Grow the window when Windows kept the physical size equal to the point size.
    ///
    /// On a 150% display egui then paints 1.5 pixels per point into that smaller
    /// window, and the right column is clipped.
    fn match_dpi(&mut self, ctx: &egui::Context) {
        if self.dpi_matched {
            return;
        }
        // Scale can show up a frame late. A 100% display never latches, and the check is tiny.
        let Some(ppp) = ctx.native_pixels_per_point() else {
            return;
        };
        if ppp < 1.2 {
            return;
        }
        let points = ctx.screen_rect().size();
        if points.x < 400.0 || points.y < 300.0 {
            return;
        }
        let hwnd = target::hwnd_of(self.shared.main_window.load(Ordering::SeqCst));
        if hwnd.0.is_null() {
            return;
        }
        if grow_window_to_pixels(hwnd, points, ppp) {
            self.dpi_matched = true;
        }
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

    /// Two equal columns that stay inside the window.
    ///
    /// `Ui::columns_const` widens both sides to whichever is wider. A vertical
    /// scroll area then grows to that width and the window clips the right side.
    /// The column rect is tall on purpose: inside a scroll area the available
    /// height is only the viewport, and a column that tall clips the lower cards
    /// so they never become something you can scroll to.
    fn two_columns(&mut self, ui: &mut Ui) {
        let gap = ui.spacing().item_spacing.x;
        let total = ui.available_width();
        let col = ((total - gap) / 2.0).max(0.0);
        let top = ui.cursor().min;
        let bottom = top.y + 100_000.0;
        let mut left = column_ui(
            ui,
            egui::Rect::from_min_max(top, egui::pos2(top.x + col, bottom)),
        );
        let mut right = column_ui(
            ui,
            egui::Rect::from_min_max(
                top + egui::vec2(col + gap, 0.0),
                egui::pos2(top.x + total, bottom),
            ),
        );
        self.running_apps(&mut left);
        self.glass_controls(&mut right);
        right.add_space(8.0);
        self.startup_card(&mut right);
        right.add_space(8.0);
        self.files_card(&mut right);
        let used = left.min_rect().height().max(right.min_rect().height());
        ui.allocate_rect(
            egui::Rect::from_min_size(top, egui::vec2(total, used)),
            Sense::hover(),
        );
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
                let full = ui.available_width();
                ui.set_max_width(full);
                ui.label(theme::card_title("Glass"));
                if let Some(process) = &self.selected {
                    ui.label(theme::muted("for"));
                    ui.add(
                        egui::Label::new(RichText::new(process).strong().color(ACCENT)).truncate(),
                    );
                }
            });
            if self.selected.is_none() {
                ui.add(
                    egui::Label::new(theme::muted(
                        "Pick an app on the left, set the look, then frost it.",
                    ))
                    .wrap(),
                );
            }
            ui.add_space(4.0);
            let mut moved = false;
            ui.horizontal_wrapped(|ui| {
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
                ui.horizontal_wrapped(|ui| {
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
                        let row_w = ui.available_width();
                        ui.set_max_width(row_w);
                        let (toggled, remove) = rule_heading(ui, &mut rule.enabled, &rule.process);
                        structural |= toggled;
                        if remove {
                            delete = Some(rule.process.clone());
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.label(theme::muted("Look"));
                            let look = look_picker(ui, &mut rule.style);
                            if look && !tweaked.iter().any(|item: &String| item == &rule.process) {
                                tweaked.push(rule.process.clone());
                            }
                        });
                        let milky = rule.style == BlurStyle::Acrylic;
                        if rule_slider_row(ui, &mut rule.transparency, &mut rule.blur, milky)
                            && !tweaked.iter().any(|item: &String| item == &rule.process)
                        {
                            tweaked.push(rule.process.clone());
                        }
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
            ui.add(
                egui::Label::new(
                    RichText::new(dir.display().to_string())
                        .monospace()
                        .color(TEXT),
                )
                .wrap(),
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
        self.match_dpi(ctx);
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
                        self.two_columns(ui);
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
    let budget = ui.available_width();
    let mut changed = false;
    ui.allocate_ui_with_layout(
        egui::vec2(budget, 0.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(budget);
            ui.label(theme::muted(label));
            ui.spacing_mut().slider_width = slider_rail_width(ui);
            changed = add(ui).changed();
        },
    );
    changed
}

/// The name stays on the left and Remove stays on the right. A long name
/// ellipsizes instead of pushing the button past the window.
fn rule_heading(ui: &mut Ui, enabled: &mut bool, process: &str) -> (bool, bool) {
    let row_w = ui.available_width();
    let mut toggled = false;
    let mut remove = false;
    ui.horizontal(|ui| {
        ui.set_width(row_w);
        let gap = ui.spacing().item_spacing.x;
        let name_w = (row_w - button_width(ui, "Remove") - gap).max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(name_w, 0.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_width(name_w);
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                let color = if *enabled { TEXT } else { MUTED };
                toggled = ui
                    .checkbox(enabled, RichText::new(process).strong().color(color))
                    .on_hover_text("Turn this rule on or off.")
                    .changed();
            },
        );
        if ui.button("Remove").clicked() {
            remove = true;
        }
    });
    (toggled, remove)
}

/// Transparency and blur on one line, each in its own half of the row.
fn rule_slider_row(ui: &mut Ui, transparency: &mut u8, blur: &mut u8, milky: bool) -> bool {
    let row_w = ui.available_width();
    let gap = ui.spacing().item_spacing.x;
    // Floor so the two halves plus the gap cannot round past the row and wrap.
    let pair_w = ((row_w - gap) / 2.0).floor().max(0.0);
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.set_width(row_w);
        changed |= slider_pair(ui, "Transparency", pair_w, |ui| {
            ui.add(
                Slider::new(
                    transparency,
                    mapping::TRANSPARENCY_MIN..=mapping::TRANSPARENCY_MAX,
                )
                .suffix("%"),
            )
        });
        changed |= slider_pair(ui, if milky { "Milkiness" } else { "Blur" }, pair_w, |ui| {
            ui.add(Slider::new(blur, mapping::BLUR_MIN..=mapping::BLUR_MAX))
        });
    });
    changed
}

fn button_width(ui: &Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text_w = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, Color32::WHITE)
        .size()
        .x;
    text_w + ui.spacing().button_padding.x * 2.0
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

/// If the client area is still the point size, size it to `points * pixels_per_point`.
/// Returns true once the window is the right size or this pass changed it.
fn grow_window_to_pixels(hwnd: HWND, points: egui::Vec2, pixels_per_point: f32) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    };

    let wanted_w = (points.x * pixels_per_point).round() as i32;
    let wanted_h = (points.y * pixels_per_point).round() as i32;
    unsafe {
        let mut client = RECT::default();
        if GetClientRect(hwnd, &mut client).is_err() {
            return false;
        }
        let client_w = client.right - client.left;
        let client_h = client.bottom - client.top;
        if (client_w - wanted_w).abs() <= 12 && (client_h - wanted_h).abs() <= 12 {
            return true;
        }
        // The broken case is a client area that matches the point size, not the pixel size.
        let width_ratio = client_w as f32 / points.x;
        let height_ratio = client_h as f32 / points.y;
        let sized_in_points =
            (0.85..1.15).contains(&width_ratio) && (0.85..1.15).contains(&height_ratio);
        if !sized_in_points {
            return false;
        }
        let mut window = RECT::default();
        if GetWindowRect(hwnd, &mut window).is_err() {
            return false;
        }
        let chrome_w = (window.right - window.left) - client_w;
        let chrome_h = (window.bottom - window.top) - client_h;
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            wanted_w + chrome_w,
            wanted_h + chrome_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .is_ok()
    }
}

fn column_ui(ui: &mut Ui, rect: egui::Rect) -> Ui {
    let mut column = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down_justified(Align::LEFT)),
    );
    column.set_max_width(rect.width());
    column.set_clip_rect(column.clip_rect().intersect(rect));
    column
}

/// A fixed-width label plus slider. The slider uses whatever width is left.
fn slider_pair(
    ui: &mut Ui,
    label: &str,
    width: f32,
    add: impl FnOnce(&mut Ui) -> egui::Response,
) -> bool {
    let width = width.min(ui.available_width()).max(0.0);
    let mut changed = false;
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(width);
            ui.label(theme::muted(label).small());
            ui.spacing_mut().slider_width = slider_rail_width(ui);
            changed = add(ui).changed();
        },
    );
    changed
}

/// Width of the number button beside a slider, measured for the widest value.
///
/// `slider_width` is only the rail. The value button is drawn after it, so a rail
/// that uses the whole remaining row paints the number outside the window.
fn slider_value_width(ui: &Ui) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text = ui
        .painter()
        .layout_no_wrap("100%".to_owned(), font, Color32::WHITE)
        .size()
        .x;
    let padded = text + ui.spacing().button_padding.x * 2.0;
    padded.max(ui.spacing().interact_size.x) + 4.0
}

/// Rail width that leaves room for the value button in the space still free.
fn slider_rail_width(ui: &Ui) -> f32 {
    let gap = ui.spacing().item_spacing.x;
    (ui.available_width() - gap - slider_value_width(ui)).max(0.0)
}

enum RowBadge {
    Pill(String, Color32),
    Count(String),
}

fn row_badges(group: &AppGroup, rule: Option<&Rule>) -> Vec<RowBadge> {
    let mut badges = Vec::new();
    if group.elevated {
        badges.push(RowBadge::Pill("Admin".to_string(), WARN));
    }
    if group.fullscreen {
        badges.push(RowBadge::Pill("Fullscreen".to_string(), WARN));
    }
    if let Some(rule) = rule {
        let look = match rule.style {
            BlurStyle::Frost => "Frost",
            BlurStyle::Acrylic => "Acrylic",
        };
        badges.push(RowBadge::Pill(
            format!("{look} · {}%", rule.transparency),
            ACCENT,
        ));
    }
    badges.push(RowBadge::Count(format!(
        "{} window{}",
        group.windows,
        if group.windows == 1 { "" } else { "s" }
    )));
    badges
}

fn draw_badge(ui: &mut Ui, badge: &RowBadge) {
    match badge {
        RowBadge::Pill(text, color) => theme::status_pill(ui, text, *color),
        RowBadge::Count(text) => {
            ui.label(theme::muted(text).small());
        }
    }
}

fn badge_width(ui: &Ui, badge: &RowBadge) -> f32 {
    match badge {
        // Pill frame: 8px padding on each side, plus a little slack so the
        // one-line layout wraps before the badges cross the row.
        RowBadge::Pill(text, _) => text_width(ui, text, 12.0) + 24.0,
        RowBadge::Count(text) => {
            text_width(ui, text, egui::TextStyle::Small.resolve(ui.style()).size)
        }
    }
}

fn text_width(ui: &Ui, text: &str, size: f32) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(size),
            Color32::WHITE,
        )
        .size()
        .x
}

fn name_block(ui: &mut Ui, group: &AppGroup, width: f32) {
    ui.vertical(|ui| {
        ui.set_max_width(width.max(0.0));
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.label(RichText::new(&group.process).strong().color(TEXT));
        ui.add(egui::Label::new(theme::muted(&group.sample_title).small()).truncate());
    });
}

fn app_row(ui: &mut Ui, group: &AppGroup, selected: bool, rule: Option<&Rule>) -> egui::Response {
    let background = ui.painter().add(Shape::Noop);
    let inner = Frame::new()
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            let row_w = ui.available_width();
            ui.set_max_width(row_w);
            let badges = row_badges(group, rule);
            let gap = ui.spacing().item_spacing.x;
            let badges_w = badges
                .iter()
                .map(|badge| badge_width(ui, badge))
                .sum::<f32>()
                + gap * badges.len().saturating_sub(1) as f32
                + 12.0;
            // Keep the name on the same line when the badges leave it room.
            if badges_w + gap + 96.0 <= row_w {
                ui.horizontal(|ui| {
                    ui.set_max_width(row_w);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        for badge in badges.iter().rev() {
                            draw_badge(ui, badge);
                        }
                        name_block(ui, group, ui.available_width());
                    });
                });
            } else {
                name_block(ui, group, row_w);
                ui.horizontal_wrapped(|ui| {
                    ui.set_max_width(row_w);
                    for badge in &badges {
                        draw_badge(ui, badge);
                    }
                });
            }
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

#[cfg(test)]
mod tests {
    use super::{app_row, labeled_slider, rule_heading, slider_pair};
    use crate::mapping::BlurStyle;
    use crate::rules::Rule;
    use crate::target::AppGroup;
    use crate::theme;
    use egui::{Slider, Ui};

    /// How far `add` paints outside a region `width` points wide.
    fn sticks_out(width: f32, add: impl FnOnce(&mut Ui)) -> f32 {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width, 800.0),
        ));
        let mut add = Some(add);
        let mut overflow = 0.0_f32;
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::new())
                .show(ctx, |ui| {
                    let bounds = ui.max_rect();
                    let inner = ui.scope(|ui| {
                        if let Some(add) = add.take() {
                            add(ui);
                        }
                    });
                    let rect = inner.response.rect;
                    overflow = (rect.right() - bounds.right())
                        .max(bounds.left() - rect.left())
                        .max(0.0);
                });
        });
        overflow
    }

    #[test]
    fn slider_rows_stay_inside_a_narrow_column() {
        let overflow = sticks_out(340.0, |ui| {
            let mut transparency = 70_u8;
            let mut blur = 100_u8;
            labeled_slider(ui, "Transparency", |ui| {
                ui.add(Slider::new(&mut transparency, 10..=70).suffix("%"))
            });
            labeled_slider(ui, "Milkiness", |ui| {
                ui.add(Slider::new(&mut blur, 1..=100))
            });
        });
        assert!(overflow <= 1.0, "glass sliders stick out by {overflow}px");
    }

    #[test]
    fn saved_rule_sliders_share_one_line() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1000.0, 400.0),
        ));
        let mut overflow = 0.0_f32;
        let mut same_line = false;
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::new())
                .show(ctx, |ui| {
                    let bounds = ui.max_rect();
                    let mut transparency = 10_u8;
                    let mut blur = 40_u8;
                    let mut tops = [0.0_f32, 0.0];
                    let inner = ui.scope(|ui| {
                        let row_w = ui.available_width();
                        let gap = ui.spacing().item_spacing.x;
                        let pair_w = ((row_w - gap) / 2.0).floor().max(0.0);
                        ui.horizontal(|ui| {
                            ui.set_width(row_w);
                            tops[0] = ui.cursor().top();
                            slider_pair(ui, "Transparency", pair_w, |ui| {
                                ui.add(Slider::new(&mut transparency, 10..=70).suffix("%"))
                            });
                            tops[1] = ui.cursor().top();
                            slider_pair(ui, "Blur", pair_w, |ui| {
                                ui.add(Slider::new(&mut blur, 1..=100))
                            });
                        });
                    });
                    let rect = inner.response.rect;
                    overflow = (rect.right() - bounds.right())
                        .max(bounds.left() - rect.left())
                        .max(0.0);
                    same_line = (tops[0] - tops[1]).abs() < 1.0;
                });
        });
        assert!(
            overflow <= 1.0,
            "saved-rule sliders stick out by {overflow}px"
        );
        assert!(same_line, "transparency and blur wrapped onto two lines");
    }

    #[test]
    fn long_rule_name_keeps_remove_inside() {
        let overflow = sticks_out(320.0, |ui| {
            let mut enabled = true;
            rule_heading(
                ui,
                &mut enabled,
                "ApplicationFrameHost.exe with a very long window name",
            );
        });
        assert!(overflow <= 1.0, "rule heading sticks out by {overflow}px");
    }

    #[test]
    fn app_rows_stay_inside_the_list() {
        let group = AppGroup {
            process: "ApplicationFrameHost.exe".to_string(),
            sample_title: "A very long window title that should stay inside the row".to_string(),
            windows: 12,
            elevated: true,
            fullscreen: true,
        };
        let rule = Rule {
            process: group.process.clone(),
            transparency: 70,
            blur: 100,
            style: BlurStyle::Acrylic,
            enabled: true,
        };
        let overflow = sticks_out(340.0, |ui| {
            app_row(ui, &group, true, Some(&rule));
        });
        assert!(overflow <= 1.0, "app row sticks out by {overflow}px");
    }
}
