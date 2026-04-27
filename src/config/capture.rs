use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Resolves the directory where session capture files are written.
/// Linux: `~/.config/nerdterm/sessions/`
/// macOS: `~/Library/Application Support/nerdterm/sessions/`
pub fn dir() -> Result<PathBuf> {
    let cfg = dirs::config_dir()
        .ok_or_else(|| anyhow!("Could not determine config directory"))?;
    Ok(cfg.join("nerdterm").join("sessions"))
}

/// Sanitize an address-book entry name into something filename-safe.
///
/// Rules (from spec):
/// - Replace any of `/ \ : ? * " < > |` or whitespace with `_`.
/// - Collapse runs of `_` to a single `_`.
/// - Trim leading and trailing `_`.
/// - Cap at 80 characters (truncate from the end).
/// - Empty result becomes the literal `session`.
/// - Other unicode is preserved as-is.
fn sanitize_entry_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_us = false;
    for ch in name.chars() {
        let bad = ch.is_whitespace()
            || matches!(ch, '/' | '\\' | ':' | '?' | '*' | '"' | '<' | '>' | '|');
        if bad {
            if !last_was_us {
                out.push('_');
                last_was_us = true;
            }
        } else {
            out.push(ch);
            last_was_us = false;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    let capped: String = trimmed.chars().take(80).collect();
    if capped.is_empty() {
        "session".into()
    } else {
        capped
    }
}

/// Pick the next non-existent `<base>.log` (or `<base>_2.log`, `_3.log`, ...) in `dir`.
/// Returns `Err` if no slot is free within the first 100 attempts.
fn next_available_path(dir: &Path, base: &str) -> Result<PathBuf> {
    for suffix in 1u32..=100 {
        let candidate = if suffix == 1 {
            dir.join(format!("{}.log", base))
        } else {
            dir.join(format!("{}_{}.log", base, suffix))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "Could not find an available capture filename in {} (tried up to _100.log)",
        dir.display()
    ))
}

pub struct CaptureFile {
    path: PathBuf,
    file: File,
    bytes_written: u64,
}

impl CaptureFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Append `data` to the capture file. Bumps `bytes_written`.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data)?;
        self.bytes_written += data.len() as u64;
        Ok(())
    }
}

/// Open a fresh capture file in the default sessions directory.
pub fn open(entry_name: &str, host: &str, port: u16) -> Result<CaptureFile> {
    let d = dir()?;
    open_in(&d, entry_name, host, port)
}

/// Open a fresh capture file in the given directory. Creates the directory
/// if missing. Writes the spec-defined header line on success.
pub fn open_in(dir: &Path, entry_name: &str, host: &str, port: u16) -> Result<CaptureFile> {
    fs::create_dir_all(dir)?;

    let now = chrono::Local::now();
    let sanitized = sanitize_entry_name(entry_name);
    let stamp = now.format("%Y-%m-%dT%H%M");
    let base = format!("{}_{}", sanitized, stamp);

    // Probe for a free name, then atomically O_EXCL create it.
    // The probe + create pair is racy in the abstract, but for a
    // single-user TUI starting captures one at a time it is fine; if
    // racing did happen, `create_new` would surface EEXIST as a
    // fail-loud error to the user.
    let path = next_available_path(dir, &base)?;
    let mut file = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            return Err(anyhow!(
                "Capture file {} already exists (race) — try again",
                path.display()
            ));
        }
        Err(e) => return Err(e.into()),
    };

    let header = format!(
        "# nerdterm session: {} {}:{} started {}\n",
        entry_name,
        host,
        port,
        now.to_rfc3339()
    );
    file.write_all(header.as_bytes())?;

    Ok(CaptureFile {
        path,
        file,
        bytes_written: header.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_tempdir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("nerdterm-cap-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_empty_becomes_session() {
        assert_eq!(sanitize_entry_name(""), "session");
    }

    #[test]
    fn sanitize_only_bad_chars_becomes_session() {
        assert_eq!(sanitize_entry_name(" / \\ : * ? \" < > | "), "session");
    }

    #[test]
    fn sanitize_replaces_spaces_with_underscore() {
        assert_eq!(sanitize_entry_name("Aardwolf MUD"), "Aardwolf_MUD");
    }

    #[test]
    fn sanitize_collapses_runs_of_bad_chars() {
        // multiple separators in a row collapse to one underscore
        assert_eq!(sanitize_entry_name("a   b///c"), "a_b_c");
    }

    #[test]
    fn sanitize_trims_leading_and_trailing_underscores() {
        assert_eq!(sanitize_entry_name("  foo  "), "foo");
        assert_eq!(sanitize_entry_name("///foo///"), "foo");
    }

    #[test]
    fn sanitize_caps_to_80_chars() {
        let long = "a".repeat(200);
        let s = sanitize_entry_name(&long);
        assert_eq!(s.chars().count(), 80);
        assert!(s.chars().all(|c| c == 'a'));
    }

    #[test]
    fn sanitize_preserves_unicode() {
        assert_eq!(sanitize_entry_name("über-café"), "über-café");
    }

    #[test]
    fn next_available_path_picks_unsuffixed_when_dir_empty() {
        let dir = unique_tempdir();
        let p = next_available_path(&dir, "abc").unwrap();
        assert_eq!(p, dir.join("abc.log"));
    }

    #[test]
    fn next_available_path_picks_suffix_2_when_unsuffixed_exists() {
        let dir = unique_tempdir();
        fs::write(dir.join("abc.log"), "").unwrap();
        let p = next_available_path(&dir, "abc").unwrap();
        assert_eq!(p, dir.join("abc_2.log"));
    }

    #[test]
    fn next_available_path_picks_suffix_3_when_two_exist() {
        let dir = unique_tempdir();
        fs::write(dir.join("abc.log"), "").unwrap();
        fs::write(dir.join("abc_2.log"), "").unwrap();
        let p = next_available_path(&dir, "abc").unwrap();
        assert_eq!(p, dir.join("abc_3.log"));
    }

    #[test]
    fn next_available_path_errors_after_100_collisions() {
        let dir = unique_tempdir();
        fs::write(dir.join("abc.log"), "").unwrap();
        for i in 2u32..=100 {
            fs::write(dir.join(format!("abc_{}.log", i)), "").unwrap();
        }
        let result = next_available_path(&dir, "abc");
        assert!(result.is_err());
    }

    #[test]
    fn open_in_creates_dir_and_writes_header() {
        let parent = unique_tempdir();
        let dir = parent.join("nested").join("sessions");
        // dir does NOT exist yet; open_in must mkdir -p
        let cap = open_in(&dir, "Test Session", "example.com", 23).unwrap();

        assert!(dir.is_dir(), "open_in must create the target directory");
        assert!(cap.path().is_file());
        assert!(
            cap.path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("Test_Session_"),
            "filename must start with sanitized entry name + underscore",
        );
        // bytes_written equals the header length
        let on_disk = fs::read(cap.path()).unwrap();
        assert_eq!(cap.bytes_written() as usize, on_disk.len());
        // header begins with the documented prefix
        let header_str = std::str::from_utf8(&on_disk).unwrap();
        assert!(header_str.starts_with("# nerdterm session: Test Session example.com:23 started "));
        assert!(header_str.ends_with('\n'));
    }

    #[test]
    fn write_appends_after_header_and_bumps_count() {
        let dir = unique_tempdir();
        let mut cap = open_in(&dir, "x", "h", 22).unwrap();
        let header_len = cap.bytes_written();

        cap.write(b"hello ").unwrap();
        cap.write(b"world").unwrap();
        cap.write(&[0x1b, b'[', b'3', b'1', b'm']).unwrap();

        let total = cap.bytes_written();
        assert_eq!(total, header_len + 6 + 5 + 5);

        // Drop closes the file; re-read and verify the suffix.
        let path = cap.path().to_path_buf();
        drop(cap);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, total);
        assert!(bytes.ends_with(b"hello world\x1b[31m"));
    }

    #[test]
    fn open_in_returns_err_when_dir_path_is_a_file() {
        let dir = unique_tempdir();
        // Make `dir/blocker` a regular file, then ask open_in to use it as a directory.
        let blocker = dir.join("blocker");
        fs::write(&blocker, "not a dir").unwrap();
        let result = open_in(&blocker, "x", "h", 22);
        assert!(result.is_err(), "open_in must fail when target is not a directory");
    }

    #[test]
    fn open_in_creates_distinct_files_when_called_twice_in_same_minute() {
        let dir = unique_tempdir();
        let cap1 = open_in(&dir, "same", "h", 22).unwrap();
        let cap2 = open_in(&dir, "same", "h", 22).unwrap();
        assert_ne!(cap1.path(), cap2.path(), "second capture must get a unique path");
        // the second one ends in _2.log
        let name2 = cap2.path().file_name().unwrap().to_str().unwrap();
        assert!(
            name2.ends_with("_2.log"),
            "expected _2.log suffix, got {}", name2,
        );
    }

    #[test]
    fn header_includes_iso8601_with_tz_offset() {
        let dir = unique_tempdir();
        let cap = open_in(&dir, "x", "h", 22).unwrap();
        let bytes = fs::read(cap.path()).unwrap();
        let header = std::str::from_utf8(&bytes).unwrap();
        // header looks like: "# nerdterm session: x h:22 started 2026-04-26T13:50:00-05:00\n"
        // The TZ offset is either +HH:MM, -HH:MM, or Z (UTC). chrono's RFC3339 picks the form.
        let tail = header.trim_end_matches('\n');
        let last_word = tail.rsplit(' ').next().unwrap();
        let ok = last_word.ends_with('Z')
            || last_word.contains('+')
            || last_word[1..].contains('-'); // skip the leading 'T...HH:' parts; the offset will have a sign
        assert!(ok, "header tail does not look ISO-8601: {:?}", tail);
        // also: the date portion is dash-separated YYYY-MM-DD
        let date_part = last_word.split('T').next().unwrap();
        assert_eq!(date_part.len(), 10, "expected YYYY-MM-DD, got {:?}", date_part);
    }
}
