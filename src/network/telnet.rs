//! Telnet protocol constants and state machine filter.
//!
//! Strips IAC sequences from the data stream, responds to negotiations,
//! and passes only clean display data through to vt100.

// Telnet commands
const IAC: u8 = 0xFF;
const WILL: u8 = 0xFB;
const WONT: u8 = 0xFC;
const DO: u8 = 0xFD;
const DONT: u8 = 0xFE;
const SB: u8 = 0xFA;
const SE: u8 = 0xF0;

// Telnet options we care about
const OPT_ECHO: u8 = 1;
const OPT_SUPPRESS_GO_AHEAD: u8 = 3;
const OPT_NAWS: u8 = 31;
const OPT_TERMINAL_TYPE: u8 = 24;

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Data,
    Iac,
    Will,
    Wont,
    Do,
    Dont,
    Sb,
    SbIac,
}

/// Output from processing a chunk of bytes.
pub struct TelnetOutput {
    /// Clean data to display (send to vt100)
    pub data: Vec<u8>,
    /// Response bytes to send back to the server
    pub response: Vec<u8>,
}

/// Build a NAWS subnegotiation payload for the given dimensions.
pub fn build_naws(cols: u16, rows: u16) -> Vec<u8> {
    let mut out = vec![IAC, SB, OPT_NAWS];
    for &byte in &cols.to_be_bytes() {
        out.push(byte);
        if byte == IAC {
            out.push(IAC);
        }
    }
    for &byte in &rows.to_be_bytes() {
        out.push(byte);
        if byte == IAC {
            out.push(IAC);
        }
    }
    out.extend_from_slice(&[IAC, SE]);
    out
}

/// Shared negotiation state between the reader (telnet filter) and the app/writer.
/// Terminal dimensions live here too so the writer can update them when the user
/// resizes and the reader picks up the latest size when answering NAWS.
pub struct TelnetFlags {
    pub naws_enabled: std::sync::atomic::AtomicBool,
    pub server_echo: std::sync::atomic::AtomicBool,
    pub cols: std::sync::atomic::AtomicU16,
    pub rows: std::sync::atomic::AtomicU16,
}

impl TelnetFlags {
    pub fn new() -> Self {
        Self {
            naws_enabled: std::sync::atomic::AtomicBool::new(false),
            server_echo: std::sync::atomic::AtomicBool::new(false),
            cols: std::sync::atomic::AtomicU16::new(80),
            rows: std::sync::atomic::AtomicU16::new(24),
        }
    }
}

pub struct TelnetFilter {
    state: State,
    sb_buf: Vec<u8>,
    naws_enabled: bool,
    flags: std::sync::Arc<TelnetFlags>,
    terminal_type: String,
}

impl TelnetFilter {
    pub fn new(
        cols: u16,
        rows: u16,
        flags: std::sync::Arc<TelnetFlags>,
        terminal_type: String,
    ) -> Self {
        flags.cols.store(cols, std::sync::atomic::Ordering::Relaxed);
        flags.rows.store(rows, std::sync::atomic::Ordering::Relaxed);
        Self {
            state: State::Data,
            sb_buf: Vec::new(),
            naws_enabled: false,
            flags,
            terminal_type,
        }
    }

    /// Process incoming bytes, returning clean data and any telnet responses.
    pub fn process(&mut self, input: &[u8]) -> TelnetOutput {
        let mut data = Vec::with_capacity(input.len());
        let mut response = Vec::new();

        for &byte in input {
            match self.state {
                State::Data => {
                    if byte == IAC {
                        self.state = State::Iac;
                    } else {
                        data.push(byte);
                    }
                }
                State::Iac => match byte {
                    IAC => {
                        // Escaped 0xFF — literal byte
                        data.push(IAC);
                        self.state = State::Data;
                    }
                    WILL => self.state = State::Will,
                    WONT => self.state = State::Wont,
                    DO => self.state = State::Do,
                    DONT => self.state = State::Dont,
                    SB => {
                        self.sb_buf.clear();
                        self.state = State::Sb;
                    }
                    _ => {
                        // Other 2-byte commands (GA, NOP, etc.) — ignore
                        self.state = State::Data;
                    }
                },
                State::Will => {
                    self.handle_will(byte, &mut response);
                    self.state = State::Data;
                }
                State::Wont => {
                    if byte == OPT_ECHO {
                        self.flags
                            .server_echo
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.state = State::Data;
                }
                State::Do => {
                    self.handle_do(byte, &mut response);
                    self.state = State::Data;
                }
                State::Dont => {
                    // Acknowledge: server tells us not to — fine
                    self.state = State::Data;
                }
                State::Sb => {
                    if byte == IAC {
                        self.state = State::SbIac;
                    } else {
                        self.sb_buf.push(byte);
                    }
                }
                State::SbIac => match byte {
                    SE => {
                        self.handle_subnegotiation(&mut response);
                        self.state = State::Data;
                    }
                    IAC => {
                        // Escaped 0xFF inside subnegotiation data
                        self.sb_buf.push(IAC);
                        self.state = State::Sb;
                    }
                    // Per RFC 855, only IAC SE and IAC IAC are valid inside SB.
                    // Any other IAC X aborts the partial subneg and X is
                    // dispatched as if it had just followed a fresh IAC.
                    WILL => {
                        self.sb_buf.clear();
                        self.state = State::Will;
                    }
                    WONT => {
                        self.sb_buf.clear();
                        self.state = State::Wont;
                    }
                    DO => {
                        self.sb_buf.clear();
                        self.state = State::Do;
                    }
                    DONT => {
                        self.sb_buf.clear();
                        self.state = State::Dont;
                    }
                    SB => {
                        self.sb_buf.clear();
                        self.state = State::Sb;
                    }
                    _ => {
                        self.sb_buf.clear();
                        self.state = State::Data;
                    }
                },
            }
        }

        TelnetOutput { data, response }
    }

    fn handle_will(&mut self, option: u8, response: &mut Vec<u8>) {
        match option {
            OPT_ECHO => {
                // Server will handle echoing (e.g. password prompts suppress echo)
                response.extend_from_slice(&[IAC, DO, option]);
                self.flags
                    .server_echo
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            OPT_SUPPRESS_GO_AHEAD => {
                response.extend_from_slice(&[IAC, DO, option]);
            }
            // Decline everything else
            _ => {
                response.extend_from_slice(&[IAC, DONT, option]);
            }
        }
    }

    fn handle_do(&mut self, option: u8, response: &mut Vec<u8>) {
        match option {
            OPT_NAWS => {
                // Accept NAWS — we'll send our window size
                response.extend_from_slice(&[IAC, WILL, OPT_NAWS]);
                self.naws_enabled = true;
                self.flags
                    .naws_enabled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.append_naws(response);
            }
            OPT_TERMINAL_TYPE => {
                // Accept terminal type negotiation
                response.extend_from_slice(&[IAC, WILL, OPT_TERMINAL_TYPE]);
            }
            // Decline everything else
            _ => {
                response.extend_from_slice(&[IAC, WONT, option]);
            }
        }
    }

    fn handle_subnegotiation(&mut self, response: &mut Vec<u8>) {
        if self.sb_buf.is_empty() {
            return;
        }
        let option = self.sb_buf[0];
        if option == OPT_TERMINAL_TYPE && self.sb_buf.len() >= 2 && self.sb_buf[1] == 1 {
            // Server asks SEND (01) — respond with the configured terminal type.
            response.extend_from_slice(&[IAC, SB, OPT_TERMINAL_TYPE, 0]); // 0 = IS
            response.extend_from_slice(self.terminal_type.as_bytes());
            response.extend_from_slice(&[IAC, SE]);
        }
    }

    fn append_naws(&self, response: &mut Vec<u8>) {
        let cols = self.flags.cols.load(std::sync::atomic::Ordering::Relaxed);
        let rows = self.flags.rows.load(std::sync::atomic::Ordering::Relaxed);
        response.extend_from_slice(&[IAC, SB, OPT_NAWS]);
        // Width and height as 2 bytes each, escaping 0xFF
        for &byte in &cols.to_be_bytes() {
            response.push(byte);
            if byte == IAC {
                response.push(IAC);
            }
        }
        for &byte in &rows.to_be_bytes() {
            response.push(byte);
            if byte == IAC {
                response.push(IAC);
            }
        }
        response.extend_from_slice(&[IAC, SE]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn new_filter(cols: u16, rows: u16) -> TelnetFilter {
        TelnetFilter::new(
            cols,
            rows,
            Arc::new(TelnetFlags::new()),
            "XTERM-256COLOR".into(),
        )
    }

    fn naws_payload_present(response: &[u8], cols: u16, rows: u16) -> bool {
        let mut needle = Vec::new();
        needle.extend_from_slice(&cols.to_be_bytes());
        needle.extend_from_slice(&rows.to_be_bytes());
        response
            .windows(needle.len())
            .any(|w| w == needle.as_slice())
    }

    #[test]
    fn naws_response_uses_initial_dimensions() {
        let mut f = new_filter(132, 50);
        let out = f.process(&[IAC, DO, OPT_NAWS]);
        assert!(
            naws_payload_present(&out.response, 132, 50),
            "expected NAWS payload with 132x50, got {:?}",
            out.response,
        );
    }

    #[test]
    fn naws_response_reflects_resized_dimensions() {
        // Production wires resize through TelnetFlags directly (the writer task
        // updates them when ConnectionCommand::Resize arrives). The filter
        // re-reads on every NAWS response.
        use std::sync::atomic::Ordering;
        let flags = Arc::new(TelnetFlags::new());
        let mut f = TelnetFilter::new(80, 24, flags.clone(), "XTERM-256COLOR".into());
        flags.cols.store(100, Ordering::Relaxed);
        flags.rows.store(40, Ordering::Relaxed);
        let out = f.process(&[IAC, DO, OPT_NAWS]);
        assert!(
            naws_payload_present(&out.response, 100, 40),
            "expected NAWS payload with 100x40 after resize, got {:?}",
            out.response,
        );
    }

    #[test]
    fn iac_inside_subneg_followed_by_garbage_does_not_swallow_later_data() {
        // Bug: in SbIac state, current code pushes IAC + the unexpected byte to
        // the subneg buffer and re-enters Sb, eating arbitrary later bytes that
        // were meant as plain data. RFC says only IAC IAC and IAC SE are valid;
        // anything else aborts the subneg.
        let mut f = new_filter(80, 24);
        let input = [
            b'A',
            IAC,
            SB,
            OPT_TERMINAL_TYPE,
            0xAB,
            IAC,
            0xF1, // NOP inside subneg — invalid, must abort cleanly
            b'B',
        ];
        let out = f.process(&input);
        assert!(
            out.data.contains(&b'A'),
            "leading 'A' missing from {:?}",
            out.data
        );
        assert!(
            out.data.contains(&b'B'),
            "data after aborted subneg was swallowed; got data={:?}",
            out.data,
        );
    }

    #[test]
    fn well_formed_subneg_still_works_after_fix() {
        // Regression guard: don't break the happy path.
        let mut f = new_filter(80, 24);
        // Server asks for terminal type: IAC SB TT SEND IAC SE
        let out = f.process(&[IAC, SB, OPT_TERMINAL_TYPE, 1, IAC, SE]);
        // Response should include "XTERM-256COLOR"
        let response_str = String::from_utf8_lossy(&out.response);
        assert!(
            response_str.contains("XTERM-256COLOR"),
            "expected terminal-type response, got {:?}",
            out.response,
        );
    }

    #[test]
    fn iac_iac_inside_subneg_is_literal_ff() {
        // Sanity: this case must keep working — IAC IAC inside SB = data byte 0xFF.
        let mut f = new_filter(80, 24);
        // Subneg with embedded literal 0xFF, then properly terminated.
        // We can't easily inspect sb_buf from outside, so just verify it doesn't
        // panic and the next data byte still flows through.
        let input = [IAC, SB, OPT_TERMINAL_TYPE, IAC, IAC, IAC, SE, b'Z'];
        let out = f.process(&input);
        assert!(out.data.contains(&b'Z'), "got data={:?}", out.data);
    }

    #[test]
    fn subneg_response_uses_configured_terminal_type() {
        let flags = Arc::new(TelnetFlags::new());
        let mut f = TelnetFilter::new(80, 24, flags, "ANSI".into());
        let out = f.process(&[IAC, SB, OPT_TERMINAL_TYPE, 1, IAC, SE]);
        let response_str = String::from_utf8_lossy(&out.response);
        assert!(
            response_str.contains("ANSI"),
            "expected configured terminal type in response, got {:?}",
            out.response,
        );
        assert!(
            !response_str.contains("XTERM-256COLOR"),
            "default terminal type leaked through; response={:?}",
            out.response,
        );
    }
}
