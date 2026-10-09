//! Colors, spacing, and the small widgets the window is built from.
//!
//! The palette matches the pump screen app: a dark plum shell, rose accent, and
//! rounded cards over the window's frosted backdrop.

use egui::{Color32, CornerRadius, Frame, Margin, RichText, Shadow, Stroke, Ui, Vec2};

pub const ACCENT: Color32 = Color32::from_rgb(196, 54, 78);
/// Behind everything when Windows cannot frost the window.
pub const BG: Color32 = Color32::from_rgb(16, 14, 20);
/// Tooltips and menus, which float over content and need to stay readable.
pub const POPUP: Color32 = Color32::from_rgb(34, 28, 36);
pub const TEXT: Color32 = Color32::from_rgb(240, 236, 238);
pub const MUTED: Color32 = Color32::from_rgb(186, 180, 186);
pub const WARN: Color32 = Color32::from_rgb(230, 186, 120);
pub const OK: Color32 = Color32::from_rgb(126, 196, 154);
pub const ROW_HOVER: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);
/// Rose at about 22% opacity, premultiplied so it can live in a const.
pub const ROW_SELECTED: Color32 = Color32::from_rgba_premultiplied(43, 12, 17, 56);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = shell_visuals();
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    ctx.set_style(style);
}

fn shell_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = POPUP;
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.faint_bg_color = Color32::from_rgba_unmultiplied(255, 255, 255, 14);
    visuals.extreme_bg_color = Color32::from_rgba_unmultiplied(18, 14, 20, 170);
    visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(176, 170, 176);
    visuals.widgets.noninteractive.bg_stroke.color =
        Color32::from_rgba_unmultiplied(255, 255, 255, 28);
    let mut idle = visuals.widgets.inactive;
    idle.bg_fill = Color32::from_rgb(58, 50, 58);
    idle.weak_bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 18);
    idle.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 36));
    idle.corner_radius = CornerRadius::same(8);
    idle.fg_stroke.color = TEXT;
    let mut hovered = idle;
    hovered.weak_bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 30);
    hovered.bg_fill = Color32::from_rgb(78, 66, 74);
    let mut active = hovered;
    active.bg_fill = ACCENT;
    active.weak_bg_fill = ACCENT;
    active.fg_stroke.color = Color32::WHITE;
    visuals.widgets.inactive = idle;
    visuals.widgets.hovered = hovered;
    visuals.widgets.active = active;
    visuals.widgets.open = hovered;
    visuals.selection.bg_fill = Color32::from_rgb(190, 52, 74);
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::from_rgb(230, 140, 152));
    visuals.hyperlink_color = Color32::from_rgb(255, 176, 186);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_stroke =
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 32));
    visuals
}

/// The tint laid over the acrylic backdrop. Opaque when Windows has no backdrop.
pub fn shell(glass: bool) -> Frame {
    let fill = if glass {
        Color32::from_rgba_unmultiplied(16, 12, 18, 150)
    } else {
        BG
    };
    Frame::new().fill(fill).inner_margin(Margin::same(16))
}

pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(Color32::from_rgba_unmultiplied(34, 28, 36, 214))
        .stroke(Stroke::new(
            1.0_f32,
            Color32::from_rgba_unmultiplied(255, 255, 255, 32),
        ))
        .corner_radius(CornerRadius::same(16))
        .inner_margin(Margin::same(14))
        .outer_margin(Margin::symmetric(4, 0))
        .shadow(Shadow {
            offset: [0, 8],
            blur: 18,
            spread: 0,
            color: Color32::from_black_alpha(80),
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner
}

pub fn card_title(text: &str) -> RichText {
    RichText::new(text).strong().size(15.0).color(TEXT)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}

pub fn status_pill(ui: &mut Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.22))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.5)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).size(12.0));
        });
}

pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
        .fill(ACCENT)
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(140.0, 34.0))
}

/// A choice in a row, filled with the accent while it is the current one.
pub fn choice(ui: &mut Ui, selected: bool, label: &str) -> egui::Response {
    let text = if selected {
        RichText::new(label).color(Color32::WHITE)
    } else {
        RichText::new(label)
    };
    let mut button = egui::Button::new(text);
    if selected {
        button = button.fill(ACCENT);
    }
    ui.add(button)
}
