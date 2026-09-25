//! Colors, spacing, and the small custom widgets the window is built from.

use egui::{
    Color32, CornerRadius, FontId, Frame, Margin, Response, RichText, Sense, Stroke, StrokeKind,
    TextStyle, Ui, Vec2,
};

// Surfaces are translucent white or accent laid over the window's frosted backdrop. Colors
// are premultiplied, so the channels of a translucent color never exceed its alpha.
pub const ACCENT: Color32 = Color32::from_rgb(125, 196, 255);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(58, 104, 150);
/// Behind everything when Windows cannot frost the window.
pub const BG: Color32 = Color32::from_rgb(16, 18, 23);
/// Tooltips and menus, which float over content and need to stay readable.
pub const POPUP: Color32 = Color32::from_rgb(30, 33, 41);
pub const CARD: Color32 = white(14);
pub const CARD_STROKE: Color32 = white(24);
pub const ROW: Color32 = white(10);
pub const ROW_HOVER: Color32 = white(20);
pub const ROW_SELECTED: Color32 = Color32::from_rgba_premultiplied(29, 46, 60, 60);
pub const TEXT: Color32 = Color32::from_rgb(236, 240, 247);
pub const MUTED: Color32 = Color32::from_rgb(160, 168, 184);
pub const WARN: Color32 = Color32::from_rgb(240, 190, 110);
pub const DANGER: Color32 = Color32::from_rgb(235, 120, 120);

const fn white(alpha: u8) -> Color32 {
    Color32::from_rgba_premultiplied(alpha, alpha, alpha, alpha)
}

pub fn line(width: f32, color: Color32) -> Stroke {
    Stroke::new(width, color)
}

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    let visuals = &mut style.visuals;
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.window_fill = POPUP;
    visuals.window_stroke = line(1.0, CARD_STROKE);
    visuals.faint_bg_color = CARD;
    visuals.extreme_bg_color = Color32::from_rgba_premultiplied(0, 0, 0, 90);
    visuals.hyperlink_color = ACCENT;
    visuals.slider_trailing_fill = true;
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.selection.bg_fill = ACCENT_DIM;
    visuals.selection.stroke = line(1.0, ACCENT);

    let widgets = &mut visuals.widgets;
    for state in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.corner_radius = CornerRadius::same(8);
    }
    widgets.noninteractive.fg_stroke = line(1.0, TEXT);
    widgets.noninteractive.bg_stroke = line(1.0, CARD_STROKE);
    widgets.inactive.weak_bg_fill = white(22);
    widgets.inactive.bg_fill = white(30);
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.inactive.fg_stroke = line(1.0, TEXT);
    widgets.hovered.weak_bg_fill = white(34);
    widgets.hovered.bg_fill = white(42);
    widgets.hovered.bg_stroke = line(1.0, ACCENT_DIM);
    widgets.hovered.fg_stroke = line(1.5, Color32::WHITE);
    widgets.active.weak_bg_fill = white(48);
    widgets.active.bg_fill = ACCENT;
    widgets.active.bg_stroke = line(1.0, ACCENT);
    widgets.active.fg_stroke = line(2.0, Color32::WHITE);

    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(8.0, 8.0);
    spacing.button_padding = Vec2::new(12.0, 5.0);
    spacing.interact_size.y = 26.0;
    spacing.slider_width = 200.0;
    spacing.slider_rail_height = 6.0;

    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(22.0)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    ctx.set_style(style);
}

pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(CARD)
        .stroke(line(1.0, CARD_STROKE))
        .corner_radius(12)
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner
}

pub fn card_title(text: &str) -> RichText {
    RichText::new(text).size(15.0).strong().color(TEXT)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}

pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(color));
        });
}

pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).strong().color(BG))
        .fill(ACCENT)
        .min_size(Vec2::new(110.0, 30.0))
}

pub fn quiet_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text)).min_size(Vec2::new(0.0, 30.0))
}

pub fn danger_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(DANGER)).fill(DANGER.gamma_multiply(0.12))
}

/// An iOS-style switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "")
    });
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool(response.id, *on);
        let radius = 0.5 * rect.height();
        let off = if response.hovered() { white(60) } else { white(42) };
        let track = lerp_color(off, ACCENT, how_on);
        ui.painter()
            .rect(rect, radius, track, Stroke::NONE, StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), radius - 3.0, Color32::WHITE);
    }
    response
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()), mix(a.a(), b.a()))
}
