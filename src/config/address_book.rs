use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::app::AddressBookEntry;

#[derive(serde::Serialize, serde::Deserialize)]
struct AddressBookFile {
    #[serde(default)]
    entries: Vec<AddressBookEntry>,
}

pub struct LoadResult {
    pub entries: Vec<AddressBookEntry>,
    pub warning: Option<String>,
}

fn config_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?;
    Ok(config_dir.join("nerdterm").join("address_book.toml"))
}

pub fn load() -> LoadResult {
    match config_path() {
        Ok(p) => load_from(&p),
        Err(e) => LoadResult {
            entries: default_entries(),
            warning: Some(format!("Could not determine config path: {}", e)),
        },
    }
}

pub fn save(entries: &[AddressBookEntry]) -> Result<()> {
    let path = config_path()?;
    save_to(&path, entries)
}

pub fn load_from(path: &Path) -> LoadResult {
    if !path.exists() {
        return LoadResult {
            entries: default_entries(),
            warning: None,
        };
    }

    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return LoadResult {
                entries: default_entries(),
                warning: Some(format!("Could not read {}: {}", path.display(), e)),
            };
        }
    };

    match toml::from_str::<AddressBookFile>(&contents) {
        Ok(file) if file.entries.is_empty() => LoadResult {
            entries: default_entries(),
            warning: None,
        },
        Ok(file) => LoadResult {
            entries: file.entries,
            warning: None,
        },
        Err(parse_err) => {
            // Quarantine the corrupt file before returning defaults — otherwise
            // the next save() would silently destroy the user's data.
            let warning = match quarantine(path) {
                Ok(backup) => format!(
                    "Could not parse {}: {}. Saved corrupt copy to {}.",
                    path.display(),
                    parse_err,
                    backup.display(),
                ),
                Err(e) => format!(
                    "Could not parse {}: {}. Failed to back it up: {}.",
                    path.display(),
                    parse_err,
                    e,
                ),
            };
            LoadResult {
                entries: default_entries(),
                warning: Some(warning),
            }
        }
    }
}

pub fn save_to(path: &Path, entries: &[AddressBookEntry]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = AddressBookFile {
        entries: entries.to_vec(),
    };
    let contents = toml::to_string_pretty(&file)?;

    // Write+rename: a crash mid-write leaves the original `path` intact
    // instead of producing a truncated file the next load() would discard.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, contents)?;
    if let Err(e) = fs::rename(&tmp, path) {
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

fn default_entries() -> Vec<AddressBookEntry> {
    use crate::app::Protocol;
    vec![
        AddressBookEntry {
            name: "CoffeeMUD".into(),
            host: "coffeemud.net".into(),
            port: 2323,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
        },
        AddressBookEntry {
            name: "Star Wars ASCII".into(),
            host: "towel.blinkenlights.nl".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
        },
        AddressBookEntry {
            name: "Aardwolf MUD".into(),
            host: "aardmud.org".into(),
            port: 4000,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
        },
        AddressBookEntry {
            name: "Legend of the Red Dragon".into(),
            host: "lord.stabs.org".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
        },
        AddressBookEntry {
            name: "Synchronet BBS".into(),
            host: "vert.synchro.net".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_tempdir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("nerdterm-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_entries() -> Vec<AddressBookEntry> {
        use crate::app::Protocol;
        vec![AddressBookEntry {
            name: "Test Host".into(),
            host: "example.com".into(),
            port: 2323,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
        }]
    }

    #[test]
    fn load_missing_file_returns_defaults_without_warning() {
        let dir = unique_tempdir();
        let path = dir.join("nope.toml");
        let result = load_from(&path);
        assert!(!result.entries.is_empty(), "should fall back to defaults");
        assert!(result.warning.is_none(), "missing file is not a warning");
    }

    #[test]
    fn load_valid_file_returns_those_entries() {
        let dir = unique_tempdir();
        let path = dir.join("ab.toml");
        save_to(&path, &sample_entries()).unwrap();
        let result = load_from(&path);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].name, "Test Host");
        assert!(result.warning.is_none());
    }

    #[test]
    fn load_malformed_file_quarantines_and_warns() {
        let dir = unique_tempdir();
        let path = dir.join("ab.toml");
        fs::write(&path, "this is not valid toml @@@ [[[\n").unwrap();
        let original_bytes = fs::read(&path).unwrap();

        let result = load_from(&path);

        assert!(!result.entries.is_empty(), "should fall back to defaults");
        assert!(result.warning.is_some(), "malformed file MUST warn");
        assert!(
            !path.exists(),
            "malformed file must be moved aside, found it still at {}",
            path.display(),
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
        let path = dir.join("nested").join("ab.toml");
        let entries = sample_entries();
        save_to(&path, &entries).unwrap();
        let result = load_from(&path);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].host, "example.com");
        assert_eq!(result.entries[0].port, 2323);
    }

    #[test]
    fn save_does_not_leave_tmp_files_behind() {
        // Atomic-save invariant: after save_to returns Ok, the only file in
        // the target directory should be the target path itself — no `.tmp`
        // sidecar that a future glob or backup script might pick up.
        let dir = unique_tempdir();
        let path = dir.join("ab.toml");
        save_to(&path, &sample_entries()).unwrap();

        let entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(
            entries,
            vec![path.clone()],
            "expected only the target file, found {:?}",
            entries,
        );
    }

    #[test]
    fn save_is_atomic_when_target_exists() {
        // Overwriting an existing file must not leave the file in an
        // intermediate state. We can't easily simulate a crash mid-write
        // in a unit test, but we can verify that the file is always either
        // the old contents or the new contents — never empty or partial.
        // After save_to returns Ok, the target must exactly match what
        // we asked to write.
        let dir = unique_tempdir();
        let path = dir.join("ab.toml");
        save_to(&path, &sample_entries()).unwrap();
        let first_size = fs::metadata(&path).unwrap().len();
        assert!(first_size > 0);

        let mut bigger = sample_entries();
        for i in 0..10 {
            bigger.push(AddressBookEntry {
                name: format!("Entry {}", i),
                host: "h".into(),
                port: 23,
                protocol: crate::app::Protocol::Telnet,
                username: None,
                terminal_type: None,
            });
        }
        save_to(&path, &bigger).unwrap();
        let second = load_from(&path);
        assert_eq!(second.entries.len(), bigger.len());
    }
}
