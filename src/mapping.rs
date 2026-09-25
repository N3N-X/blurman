//! Slider ranges and the decision of how a window is put back.

/// See-through percent. 10 keeps the app readable, 70 is the most glass we allow.
pub const TRANSPARENCY_MIN: u8 = 10;
pub const TRANSPARENCY_MAX: u8 = 70;
pub const TRANSPARENCY_DEFAULT: u8 = 30;

/// Blur strength. Never zero: a rule that is on always blurs what shows through.
pub const BLUR_MIN: u8 = 1;
pub const BLUR_MAX: u8 = 100;
pub const BLUR_DEFAULT: u8 = 40;

const BLUR_RADIUS_MIN: f32 = 4.0;
const BLUR_RADIUS_MAX: f32 = 48.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedStyle {
    pub exstyle: i64,
    pub was_layered: bool,
    pub alpha: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAction {
    /// The window was already layered. Put its previous alpha back and leave the style bit.
    KeepLayered { alpha: u8 },
    /// We added the layered bit. Take it off and ask the window to repaint.
    ClearLayered,
}

pub fn clamp_transparency(value: u8) -> u8 {
    value.clamp(TRANSPARENCY_MIN, TRANSPARENCY_MAX)
}

pub fn clamp_blur(value: u8) -> u8 {
    value.clamp(BLUR_MIN, BLUR_MAX)
}

/// Higher transparency means a lower window alpha (more of the glass shows through).
pub fn transparency_to_alpha(transparency: u8) -> u8 {
    let t = clamp_transparency(transparency) as f32 / 100.0;
    (255.0 * (1.0 - t)).round() as u8
}

/// Maps the blur slider onto the Gaussian standard deviation in DIPs.
/// The visible radius is about three times this value, so the slider covers
/// roughly 12px to 144px of frost while the effect itself stays in a stable range.
pub fn blur_strength_to_radius(strength: u8) -> f32 {
    let s = clamp_blur(strength) as f32;
    let span = (BLUR_MAX - BLUR_MIN) as f32;
    BLUR_RADIUS_MIN + (s - BLUR_MIN as f32) * (BLUR_RADIUS_MAX - BLUR_RADIUS_MIN) / span
}

/// Fallback acrylic has no radius. The slider instead picks how milky the tint is.
pub fn blur_strength_to_tint_alpha(strength: u8) -> u8 {
    let s = clamp_blur(strength) as f32;
    let span = (BLUR_MAX - BLUR_MIN) as f32;
    let alpha = 28.0 + (s - BLUR_MIN as f32) * (200.0 - 28.0) / span;
    alpha.round() as u8
}

pub fn restore_action(saved: &SavedStyle) -> RestoreAction {
    if saved.was_layered {
        RestoreAction::KeepLayered { alpha: saved.alpha }
    } else {
        RestoreAction::ClearLayered
    }
}

/// "chrome", "chrome.exe", and a full path all become "chrome.exe".
pub fn normalize_process(name: &str) -> String {
    let trimmed = name.trim().trim_matches('"');
    let file = trimmed
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    if file.is_empty() {
        return String::new();
    }
    if file.to_ascii_lowercase().ends_with(".exe") {
        file.to_string()
    } else {
        format!("{file}.exe")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparency_stays_visible_and_orders_correctly() {
        let solid = transparency_to_alpha(TRANSPARENCY_MIN);
        let glassy = transparency_to_alpha(TRANSPARENCY_MAX);
        assert!(solid > glassy);
        assert!(glassy >= 76);
        assert!(solid < 255);
        assert_eq!(transparency_to_alpha(0), solid);
        assert_eq!(transparency_to_alpha(100), glassy);
    }

    #[test]
    fn blur_slider_never_reaches_a_sharp_backdrop() {
        let low = blur_strength_to_radius(BLUR_MIN);
        let high = blur_strength_to_radius(BLUR_MAX);
        assert!((low - 4.0).abs() < 0.01);
        assert!((high - 48.0).abs() < 0.01);
        assert!(low > 0.0);
        assert!(high > low);
        assert!(blur_strength_to_tint_alpha(BLUR_MIN) < blur_strength_to_tint_alpha(BLUR_MAX));
    }

    #[test]
    fn restore_respects_windows_that_were_already_layered() {
        let layered = SavedStyle {
            exstyle: 0x0008_0000,
            was_layered: true,
            alpha: 200,
        };
        assert_eq!(
            restore_action(&layered),
            RestoreAction::KeepLayered { alpha: 200 }
        );
        let plain = SavedStyle {
            exstyle: 0x0000_0100,
            was_layered: false,
            alpha: 255,
        };
        assert_eq!(restore_action(&plain), RestoreAction::ClearLayered);
    }

    #[test]
    fn process_names_normalize() {
        assert_eq!(normalize_process("chrome"), "chrome.exe");
        assert_eq!(normalize_process("C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"), "chrome.exe");
        assert_eq!(normalize_process("notepad.EXE"), "notepad.EXE");
    }
}
