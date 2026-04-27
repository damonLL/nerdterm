use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
                let warning = match quarantine(p) {
                    Ok(backup) => format!(
                        "settings file at {} failed to parse ({}); quarantined to {}, using defaults",
                        p.display(),
                        e,
                        backup.display(),
                    ),
                    Err(qe) => format!(
                        "settings file at {} failed to parse ({}); failed to quarantine: {}, using defaults",
                        p.display(),
                        e,
                        qe,
                    ),
                };
                LoadedSettings {
                    settings: Settings::default(),
                    warning: Some(warning),
                }
            }
        },
        Err(_) => LoadedSettings {
            settings: Settings::default(),
            warning: None,
        },
    }
}

// Consumed by tests today; Task 6 (settings popup) wires `save` into App.
#[allow(dead_code)]
pub fn save(s: &Settings) -> Result<()> {
    let p = path()?;
    save_to(&p, s)
}

// Same as `save`: tests exercise it; Task 6 wires it in via App.
#[allow(dead_code)]
pub fn save_to(p: &Path, s: &Settings) -> Result<()> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(s)?;
    let mut tmp = p.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text)?;
    if let Err(e) = fs::rename(&tmp, p) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn quarantine(path: &Path) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut backup = path.as_os_str().to_owned();
    backup.push(format!(".corrupt-{}", ts));
    let backup = PathBuf::from(backup);
    fs::rename(path, &backup)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_tempdir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "nerdterm-settings-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_returns_defaults_silently() {
        let dir = unique_tempdir();
        let p = dir.join("settings.toml");
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 1000);
        assert_eq!(loaded.settings.default_input_mode, InputMode::LineBuffered);
        assert_eq!(loaded.settings.terminal_type, "xterm-256color");
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn empty_file_returns_defaults_silently() {
        let dir = unique_tempdir();
        let p = dir.join("settings.toml");
        fs::write(&p, "").unwrap();
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 1000);
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn partial_file_fills_unspecified_fields_from_defaults() {
        let dir = unique_tempdir();
        let p = dir.join("settings.toml");
        fs::write(&p, "scrollback_lines = 5000\n").unwrap();
        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 5000);
        assert_eq!(loaded.settings.default_input_mode, InputMode::LineBuffered);
        assert_eq!(loaded.settings.terminal_type, "xterm-256color");
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn input_mode_accepts_line_and_character_strings() {
        let dir = unique_tempdir();
        let p = dir.join("settings.toml");
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
    }

    #[test]
    fn corrupt_file_is_quarantined_and_warning_returned() {
        let dir = unique_tempdir();
        let p = dir.join("settings.toml");
        fs::write(&p, "scrollback_lines = \"not a number\"\n").unwrap();
        let original_bytes = fs::read(&p).unwrap();

        let loaded = load_from(&p);
        assert_eq!(loaded.settings.scrollback_lines, 1000);
        assert!(
            loaded.warning.is_some(),
            "expected a warning for corrupt file"
        );
        assert!(
            !p.exists(),
            "corrupt file must be moved aside; still found at {}",
            p.display(),
        );
        let preserved = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).any(|e| {
            fs::read(e.path())
                .map(|b| b == original_bytes)
                .unwrap_or(false)
        });
        assert!(preserved, "original bytes must be preserved on disk");
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = unique_tempdir();
        let p = dir.join("settings.toml");
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
    }

    #[test]
    fn save_does_not_leave_tmp_files_behind() {
        let dir = unique_tempdir();
        let p = dir.join("settings.toml");
        save_to(&p, &Settings::default()).unwrap();
        let entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(
            entries,
            vec![p.clone()],
            "expected only the target file, found {:?}",
            entries,
        );
    }
}
