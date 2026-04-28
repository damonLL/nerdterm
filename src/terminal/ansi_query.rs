//! Side-channel scanner for ANSI/VT control queries that expect a response
//! (DSR, primary DA). The scanner observes the inbound byte stream without
//! modifying it — the caller still feeds the full buffer to the emulator,
//! which silently consumes these queries. Detected queries are returned so
//! the caller can write the appropriate reply back to the server.
//!
//! Without this responder, BBSes and login flows that probe the terminal
//! with `ESC[6n` (cursor position) hit a timeout and fall through to a
//! visible "press a key" prompt. xterm and the system `telnet`/`ssh`
//! clients reply automatically; this module brings nerdterm to parity.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AnsiQuery {
    /// `ESC[6n` — Cursor Position Report request. Reply: `ESC[<row>;<col>R`.
    CursorPositionReport,
    /// `ESC[5n` — Device Status Report. Reply: `ESC[0n` (OK).
    DeviceStatusOk,
    /// `ESC[c` / `ESC[0c` — Primary Device Attributes. Reply: `ESC[?1;2c`.
    PrimaryDeviceAttributes,
}

enum State {
    Normal,
    AfterEsc,
    InCsi { params: Vec<u8> },
}

pub struct AnsiQueryScanner {
    state: State,
}

impl AnsiQueryScanner {
    pub fn new() -> Self {
        Self {
            state: State::Normal,
        }
    }

    /// Feed bytes through the scanner. State persists across calls so
    /// sequences split across two `read()`s still match.
    pub fn scan(&mut self, data: &[u8]) -> Vec<AnsiQuery> {
        let mut out = Vec::new();
        for &b in data {
            self.state = match std::mem::replace(&mut self.state, State::Normal) {
                State::Normal => {
                    if b == 0x1b {
                        State::AfterEsc
                    } else {
                        State::Normal
                    }
                }
                State::AfterEsc => {
                    if b == b'[' {
                        State::InCsi { params: Vec::new() }
                    } else if b == 0x1b {
                        State::AfterEsc
                    } else {
                        State::Normal
                    }
                }
                State::InCsi { mut params } => {
                    if b == 0x1b {
                        // A fresh ESC mid-sequence aborts the current CSI.
                        State::AfterEsc
                    } else if b.is_ascii_digit() || b == b';' {
                        params.push(b);
                        State::InCsi { params }
                    } else if (0x40..=0x7e).contains(&b) {
                        if let Some(q) = match_query(&params, b) {
                            out.push(q);
                        }
                        State::Normal
                    } else {
                        // Intermediate (0x20..0x2f) or private-marker
                        // (0x3c..0x3f, e.g. '?') — not one of our queries.
                        State::Normal
                    }
                }
            };
        }
        out
    }
}

impl Default for AnsiQueryScanner {
    fn default() -> Self {
        Self::new()
    }
}

fn match_query(params: &[u8], final_byte: u8) -> Option<AnsiQuery> {
    match final_byte {
        b'n' => match params {
            b"6" => Some(AnsiQuery::CursorPositionReport),
            b"5" => Some(AnsiQuery::DeviceStatusOk),
            _ => None,
        },
        b'c' => match params {
            b"" | b"0" => Some(AnsiQuery::PrimaryDeviceAttributes),
            _ => None,
        },
        _ => None,
    }
}

/// Build a CPR response. `row` and `col` are 0-based (matching
/// `vt100::Screen::cursor_position`); the wire format is 1-based.
pub fn cpr_response(row: u16, col: u16) -> Vec<u8> {
    format!(
        "\x1b[{};{}R",
        row.saturating_add(1),
        col.saturating_add(1)
    )
    .into_bytes()
}

pub fn dsr_ok_response() -> Vec<u8> {
    b"\x1b[0n".to_vec()
}

pub fn da_response() -> Vec<u8> {
    // VT102 / xterm primary DA — "advanced video terminal".
    b"\x1b[?1;2c".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cpr_query() {
        let mut s = AnsiQueryScanner::new();
        assert_eq!(s.scan(b"\x1b[6n"), vec![AnsiQuery::CursorPositionReport]);
    }

    #[test]
    fn detects_dsr_status_query() {
        let mut s = AnsiQueryScanner::new();
        assert_eq!(s.scan(b"\x1b[5n"), vec![AnsiQuery::DeviceStatusOk]);
    }

    #[test]
    fn detects_da_query_no_param() {
        let mut s = AnsiQueryScanner::new();
        assert_eq!(
            s.scan(b"\x1b[c"),
            vec![AnsiQuery::PrimaryDeviceAttributes],
        );
    }

    #[test]
    fn detects_da_query_zero_param() {
        let mut s = AnsiQueryScanner::new();
        assert_eq!(
            s.scan(b"\x1b[0c"),
            vec![AnsiQuery::PrimaryDeviceAttributes],
        );
    }

    #[test]
    fn ignores_unrelated_csi() {
        let mut s = AnsiQueryScanner::new();
        // Clear screen, set color, move cursor — none should produce a query.
        assert!(s.scan(b"\x1b[2J\x1b[31m\x1b[24;1H").is_empty());
    }

    #[test]
    fn detects_query_among_other_data() {
        let mut s = AnsiQueryScanner::new();
        assert_eq!(
            s.scan(b"hello\x1b[6nworld"),
            vec![AnsiQuery::CursorPositionReport],
        );
    }

    #[test]
    fn detects_query_split_across_reads() {
        let mut s = AnsiQueryScanner::new();
        assert!(s.scan(b"\x1b").is_empty());
        assert!(s.scan(b"[").is_empty());
        assert!(s.scan(b"6").is_empty());
        assert_eq!(s.scan(b"n"), vec![AnsiQuery::CursorPositionReport]);
    }

    #[test]
    fn detects_multiple_queries_in_one_buffer() {
        let mut s = AnsiQueryScanner::new();
        assert_eq!(
            s.scan(b"\x1b[6n\x1b[5n"),
            vec![
                AnsiQuery::CursorPositionReport,
                AnsiQuery::DeviceStatusOk,
            ],
        );
    }

    #[test]
    fn does_not_match_dsr_with_unsupported_param() {
        let mut s = AnsiQueryScanner::new();
        assert!(s.scan(b"\x1b[7n").is_empty());
    }

    #[test]
    fn does_not_match_dec_private_query() {
        // ESC[?6n is DEC private (asks about mode 6, origin), NOT CPR.
        let mut s = AnsiQueryScanner::new();
        assert!(s.scan(b"\x1b[?6n").is_empty());
    }

    #[test]
    fn does_not_match_multi_param_dsr() {
        // ESC[6;7n is not a recognized CPR variant.
        let mut s = AnsiQueryScanner::new();
        assert!(s.scan(b"\x1b[6;7n").is_empty());
    }

    #[test]
    fn esc_aborts_in_progress_csi() {
        // Mid-sequence ESC must restart, not poison the next CSI.
        let mut s = AnsiQueryScanner::new();
        assert_eq!(
            s.scan(b"\x1b[\x1b[6n"),
            vec![AnsiQuery::CursorPositionReport],
        );
    }

    #[test]
    fn cpr_response_is_one_based() {
        assert_eq!(cpr_response(0, 0), b"\x1b[1;1R");
        assert_eq!(cpr_response(23, 79), b"\x1b[24;80R");
    }

    #[test]
    fn dsr_ok_response_is_csi_0n() {
        assert_eq!(dsr_ok_response(), b"\x1b[0n");
    }

    #[test]
    fn da_response_is_vt102_advanced() {
        assert_eq!(da_response(), b"\x1b[?1;2c");
    }
}
