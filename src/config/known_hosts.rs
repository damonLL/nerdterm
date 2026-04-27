use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Trusted,
    Unknown,
    Mismatch { stored: String },
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct KnownHostsFile {
    #[serde(default)]
    hosts: Vec<HostKey>,
}

#[derive(Default)]
pub struct KnownHosts {
    entries: Vec<HostKey>,
}

pub struct LoadResult {
    pub known_hosts: KnownHosts,
    pub warning: Option<String>,
}

impl KnownHosts {
    pub fn verify(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        fingerprint: &str,
    ) -> Verdict {
        for entry in &self.entries {
            if entry.host == host && entry.port == port && entry.key_type == key_type {
                if entry.fingerprint == fingerprint {
                    return Verdict::Trusted;
                }
                return Verdict::Mismatch { stored: entry.fingerprint.clone() };
            }
        }
        Verdict::Unknown
    }

    pub fn add(&mut self, entry: HostKey) {
        self.entries.push(entry);
    }

    pub fn path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?;
        Ok(config_dir.join("nerdterm").join("known_hosts.toml"))
    }
}

pub fn load() -> LoadResult {
    match KnownHosts::path() {
        Ok(p) => load_from(&p),
        Err(e) => LoadResult {
            known_hosts: KnownHosts::default(),
            warning: Some(format!("Could not determine known_hosts path: {}", e)),
        },
    }
}

pub fn save(kh: &KnownHosts) -> Result<()> {
    let path = KnownHosts::path()?;
    save_to(&path, kh)
}

pub fn load_from(path: &Path) -> LoadResult {
    if !path.exists() {
        return LoadResult { known_hosts: KnownHosts::default(), warning: None };
    }

    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return LoadResult {
                known_hosts: KnownHosts::default(),
                warning: Some(format!("Could not read {}: {}", path.display(), e)),
            };
        }
    };

    match toml::from_str::<KnownHostsFile>(&contents) {
        Ok(file) => LoadResult {
            known_hosts: KnownHosts { entries: file.hosts },
            warning: None,
        },
        Err(parse_err) => {
            let warning = match quarantine(path) {
                Ok(backup) => format!(
                    "Could not parse {}: {}. Saved corrupt copy to {}.",
                    path.display(), parse_err, backup.display(),
                ),
                Err(e) => format!(
                    "Could not parse {}: {}. Failed to back it up: {}.",
                    path.display(), parse_err, e,
                ),
            };
            LoadResult { known_hosts: KnownHosts::default(), warning: Some(warning) }
        }
    }
}

pub fn save_to(path: &Path, kh: &KnownHosts) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = KnownHostsFile { hosts: kh.entries.clone() };
    let contents = toml::to_string_pretty(&file)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(host: &str, port: u16, kt: &str, fp: &str) -> HostKey {
        HostKey {
            host: host.into(),
            port,
            key_type: kt.into(),
            fingerprint: fp.into(),
        }
    }

    #[test]
    fn verify_returns_trusted_on_exact_match() {
        let mut kh = KnownHosts::default();
        kh.add(entry("h", 22, "ssh-ed25519", "SHA256:abc"));
        assert_eq!(
            kh.verify("h", 22, "ssh-ed25519", "SHA256:abc"),
            Verdict::Trusted,
        );
    }

    #[test]
    fn verify_returns_unknown_on_empty_store() {
        let kh = KnownHosts::default();
        assert_eq!(
            kh.verify("h", 22, "ssh-ed25519", "SHA256:abc"),
            Verdict::Unknown,
        );
    }

    #[test]
    fn verify_returns_unknown_when_port_differs() {
        let mut kh = KnownHosts::default();
        kh.add(entry("h", 22, "ssh-ed25519", "SHA256:abc"));
        assert_eq!(
            kh.verify("h", 2222, "ssh-ed25519", "SHA256:abc"),
            Verdict::Unknown,
        );
    }

    #[test]
    fn verify_returns_unknown_when_key_type_differs() {
        let mut kh = KnownHosts::default();
        kh.add(entry("h", 22, "ssh-ed25519", "SHA256:abc"));
        assert_eq!(
            kh.verify("h", 22, "ssh-rsa", "SHA256:abc"),
            Verdict::Unknown,
        );
    }

    #[test]
    fn verify_returns_mismatch_when_fingerprint_differs() {
        let mut kh = KnownHosts::default();
        kh.add(entry("h", 22, "ssh-ed25519", "SHA256:abc"));
        assert_eq!(
            kh.verify("h", 22, "ssh-ed25519", "SHA256:xyz"),
            Verdict::Mismatch { stored: "SHA256:abc".into() },
        );
    }

    #[test]
    fn add_then_verify_returns_trusted() {
        let mut kh = KnownHosts::default();
        kh.add(entry("h", 22, "ssh-ed25519", "SHA256:abc"));
        assert_eq!(
            kh.verify("h", 22, "ssh-ed25519", "SHA256:abc"),
            Verdict::Trusted,
        );
    }

    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_tempdir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("nerdterm-kh-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_from_missing_file_returns_empty_no_warning() {
        let dir = unique_tempdir();
        let path = dir.join("nope.toml");
        let result = load_from(&path);
        assert!(result.known_hosts.entries.is_empty());
        assert!(result.warning.is_none());
    }

    #[test]
    fn save_then_load_preserves_all_fields() {
        let dir = unique_tempdir();
        let path = dir.join("nested").join("kh.toml");
        let mut kh = KnownHosts::default();
        kh.add(entry("bbs.example.com", 22, "ssh-ed25519", "SHA256:abc"));
        kh.add(entry("bbs.example.com", 22, "ssh-rsa", "SHA256:def"));
        save_to(&path, &kh).unwrap();

        let loaded = load_from(&path);
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.known_hosts.entries.len(), 2);
        assert_eq!(loaded.known_hosts.entries[0].host, "bbs.example.com");
        assert_eq!(loaded.known_hosts.entries[0].port, 22);
        assert_eq!(loaded.known_hosts.entries[0].key_type, "ssh-ed25519");
        assert_eq!(loaded.known_hosts.entries[0].fingerprint, "SHA256:abc");
        assert_eq!(loaded.known_hosts.entries[1].key_type, "ssh-rsa");
    }

    #[test]
    fn save_does_not_leave_tmp_file_behind() {
        let dir = unique_tempdir();
        let path = dir.join("kh.toml");
        let mut kh = KnownHosts::default();
        kh.add(entry("h", 22, "ssh-ed25519", "SHA256:abc"));
        save_to(&path, &kh).unwrap();

        let entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(
            entries, vec![path.clone()],
            "expected only the target file, found {:?}", entries,
        );
    }

    #[test]
    fn load_from_corrupt_file_quarantines_and_warns() {
        let dir = unique_tempdir();
        let path = dir.join("kh.toml");
        fs::write(&path, "not toml @@@ [[[\n").unwrap();
        let original_bytes = fs::read(&path).unwrap();

        let result = load_from(&path);

        assert!(result.known_hosts.entries.is_empty());
        assert!(result.warning.is_some(), "corrupt file MUST warn");
        assert!(!path.exists(), "corrupt file must be moved aside");
        let preserved = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| fs::read(e.path()).map(|b| b == original_bytes).unwrap_or(false));
        assert!(preserved, "original bytes must be preserved on disk");
    }
}
