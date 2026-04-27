#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::app::InputMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: usize,
    #[serde(default = "default_input_mode")]
    pub default_input_mode: InputMode,
    #[serde(default = "default_terminal_type")]
    pub terminal_type: String,
}

fn default_scrollback_lines() -> usize {
    1000
}
fn default_input_mode() -> InputMode {
    InputMode::LineBuffered
}
fn default_terminal_type() -> String {
    "xterm-256color".into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scrollback_lines: default_scrollback_lines(),
            default_input_mode: default_input_mode(),
            terminal_type: default_terminal_type(),
        }
    }
}

pub struct LoadedSettings {
    pub settings: Settings,
    pub warning: Option<String>,
}

pub fn path() -> Result<PathBuf> {
    let cfg = dirs::config_dir().ok_or_else(|| anyhow!("Could not determine config directory"))?;
    Ok(cfg.join("nerdterm").join("settings.toml"))
}

pub fn load() -> LoadedSettings {
    match path() {
        Ok(p) => load_from(&p),
        Err(_) => LoadedSettings {
            settings: Settings::default(),
            warning: None,
        },
    }
}

pub fn load_from(p: &Path) -> LoadedSettings {
    match fs::read_to_string(p) {
        Ok(text) => match toml::from_str::<Settings>(&text) {
            Ok(s) => LoadedSettings {
                settings: s,
                warning: None,
            },
            Err(e) => {
                let bad = p.with_extension("toml.bad");
                let _ = fs::rename(p, &bad);
                LoadedSettings {
                    settings: Settings::default(),
                    warning: Some(format!(
                        "settings file at {} failed to parse ({}); quarantined to {}, using defaults",
                        p.display(),
                        e,
                        bad.display(),
                    )),
                }
            }
        },
        Err(_) => LoadedSettings {
            settings: Settings::default(),
            warning: None,
        },
    }
}

pub fn save(s: &Settings) -> Result<()> {
    let p = path()?;
    save_to(&p, s)
}

pub fn save_to(p: &Path, s: &Settings) -> Result<()> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(s)?;
    let tmp = p.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_path() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "nerdterm-settings-test-{}-{}.toml",
            std::process::id(),
            n
        ))
    }

    #[test]
    fn missing_file_returns_defaults_silently() {
        let p = unique_path();
        let _ = fs::remove_file(&p);
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 1000);
        assert_eq!(loaded.settings.default_input_mode, InputMode::LineBuffered);
        assert_eq!(loaded.settings.terminal_type, "xterm-256color");
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn empty_file_returns_defaults_silently() {
        let p = unique_path();
        fs::write(&p, "").unwrap();
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 1000);
        assert!(loaded.warning.is_none());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn partial_file_fills_unspecified_fields_from_defaults() {
        let p = unique_path();
        fs::write(&p, "scrollback_lines = 5000\n").unwrap();
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 5000);
        assert_eq!(loaded.settings.default_input_mode, InputMode::LineBuffered);
        assert_eq!(loaded.settings.terminal_type, "xterm-256color");
        assert!(loaded.warning.is_none());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn input_mode_accepts_line_and_character_strings() {
        let p = unique_path();
        fs::write(&p, "default_input_mode = \"character\"\n").unwrap();
        assert_eq!(
            load_from(&p).settings.default_input_mode,
            InputMode::Character
        );
        fs::write(&p, "default_input_mode = \"line\"\n").unwrap();
        assert_eq!(
            load_from(&p).settings.default_input_mode,
            InputMode::LineBuffered
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn corrupt_file_is_quarantined_and_warning_returned() {
        let p = unique_path();
        fs::write(&p, "scrollback_lines = \"not a number\"\n").unwrap();
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 1000);
        assert!(
            loaded.warning.is_some(),
            "expected a warning for corrupt file"
        );
        let bad = p.with_extension("toml.bad");
        assert!(
            bad.exists(),
            "expected quarantine file at {}",
            bad.display()
        );
        let _ = fs::remove_file(&p);
        let _ = fs::remove_file(&bad);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let p = unique_path();
        let s = Settings {
            scrollback_lines: 4242,
            default_input_mode: InputMode::Character,
            terminal_type: "ANSI".into(),
        };
        save_to(&p, &s).unwrap();
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 4242);
        assert_eq!(loaded.settings.default_input_mode, InputMode::Character);
        assert_eq!(loaded.settings.terminal_type, "ANSI");
        assert!(loaded.warning.is_none());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn save_does_not_leave_tmp_files_behind() {
        let p = unique_path();
        save_to(&p, &Settings::default()).unwrap();
        let tmp = p.with_extension("toml.tmp");
        assert!(!tmp.exists(), "tmp file leaked at {}", tmp.display());
        let _ = fs::remove_file(&p);
    }
}
