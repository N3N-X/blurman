//! Rules the user asked for, plus the original style of every window we touched.

use crate::mapping::{self, SavedStyle};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub process: String,
    pub transparency: u8,
    pub blur: u8,
    pub enabled: bool,
    /// Keep text and images solid: only the app's background turns to glass.
    #[serde(default)]
    pub solid_text: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Store {
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Store {
    pub fn normalize(&mut self) {
        for rule in &mut self.rules {
            rule.process = mapping::normalize_process(&rule.process);
            rule.transparency = mapping::clamp_transparency(rule.transparency);
            rule.blur = mapping::clamp_blur(rule.blur);
        }
        self.rules.retain(|rule| !rule.process.is_empty());
    }

    pub fn rule(&self, process: &str) -> Option<&Rule> {
        let want = mapping::normalize_process(process);
        self.rules
            .iter()
            .find(|rule| rule.process.eq_ignore_ascii_case(&want))
    }

    pub fn upsert(&mut self, mut rule: Rule) {
        rule.process = mapping::normalize_process(&rule.process);
        rule.transparency = mapping::clamp_transparency(rule.transparency);
        rule.blur = mapping::clamp_blur(rule.blur);
        if let Some(existing) = self
            .rules
            .iter_mut()
            .find(|item| item.process.eq_ignore_ascii_case(&rule.process))
        {
            *existing = rule;
        } else if !rule.process.is_empty() {
            self.rules.push(rule);
        }
    }

    pub fn remove(&mut self, process: &str) -> bool {
        let want = mapping::normalize_process(process);
        let before = self.rules.len();
        self.rules
            .retain(|rule| !rule.process.eq_ignore_ascii_case(&want));
        before != self.rules.len()
    }

    pub fn clear(&mut self) {
        self.rules.clear();
        self.paused = false;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub process_start: u64,
    pub process: String,
    pub original_exstyle: i64,
    pub original_layered: bool,
    pub original_alpha: u8,
}

impl PersistedWindow {
    pub fn saved_style(&self) -> SavedStyle {
        SavedStyle {
            exstyle: self.original_exstyle,
            was_layered: self.original_layered,
            alpha: self.original_alpha,
        }
    }
}

/// Window behavior. Start with Windows lives in the registry, not here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub close_to_tray: bool,
    /// Keep solid text running while a fullscreen game is in front instead of pausing it.
    #[serde(default)]
    pub solid_text_in_fullscreen: bool,
}

pub fn load_settings() -> Settings {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let text = serde_json::to_string_pretty(settings).map_err(|err| err.to_string())?;
    write_atomic(&settings_path(), &text)
}

pub fn settings_path() -> PathBuf {
    app_dir().join("settings.json")
}

pub fn app_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("Blurman")
}

pub fn rules_path() -> PathBuf {
    app_dir().join("rules.json")
}

pub fn state_path() -> PathBuf {
    app_dir().join("state.json")
}

pub fn load() -> Store {
    let mut store = match fs::read_to_string(rules_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Store::default(),
    };
    store.normalize();
    store
}

pub fn save(store: &Store) -> Result<(), String> {
    let text = serde_json::to_string_pretty(store).map_err(|err| err.to_string())?;
    write_atomic(&rules_path(), &text)
}

pub fn load_state() -> Vec<PersistedWindow> {
    match fs::read_to_string(state_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_state(windows: &[PersistedWindow]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(windows).map_err(|err| err.to_string())?;
    write_atomic(&state_path(), &text)
}

/// Readers never see a half-written file, so a reload mid-save cannot wipe the rules.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    }
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text).map_err(|err| err.to_string())?;
    fs::rename(&temp, path).map_err(|err| err.to_string())
}

pub fn new_rule(process: &str, transparency: u8, blur: u8, solid_text: bool) -> Rule {
    Rule {
        process: mapping::normalize_process(process),
        transparency: mapping::clamp_transparency(transparency),
        blur: mapping::clamp_blur(blur),
        enabled: true,
        solid_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_matches_names_case_insensitively() {
        let mut store = Store::default();
        store.upsert(new_rule("Chrome", 30, 40, false));
        store.upsert(new_rule("chrome.exe", 50, 60, true));
        assert_eq!(store.rules.len(), 1);
        assert_eq!(store.rules[0].transparency, 50);
        assert_eq!(store.rules[0].blur, 60);
        assert!(store.rules[0].solid_text);
        assert!(store.remove("CHROME.EXE"));
        assert!(store.rules.is_empty());
    }

    #[test]
    fn stored_values_are_clamped() {
        let mut store: Store = serde_json::from_str(
            r#"{"rules":[{"process":"notepad","transparency":250,"blur":0,"enabled":true},
                         {"process":"  ","transparency":30,"blur":40,"enabled":true}]}"#,
        )
        .unwrap();
        store.normalize();
        assert_eq!(store.rules.len(), 1);
        assert_eq!(store.rules[0].process, "notepad.exe");
        assert_eq!(store.rules[0].transparency, mapping::TRANSPARENCY_MAX);
        assert_eq!(store.rules[0].blur, mapping::BLUR_MIN);
        assert!(!store.rules[0].solid_text);
        assert!(!store.paused);
    }
}
