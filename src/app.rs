use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
    MouseEventKind,
};
use tokio::sync::mpsc;

use crate::config;
use crate::events::{AppEvent, ConnectionCommand};
use crate::network;
use crate::network::telnet::TelnetFlags;
use crate::terminal::ansi_query::{
    AnsiQuery, AnsiQueryScanner, cpr_response, da_response, dsr_ok_response,
};
use crate::terminal::emulator::TerminalEmulator;
use tokio_util::sync::CancellationToken;

/// Lines scrolled per Shift+PgUp/PgDn (and PgUp/PgDn in line-buffered mode).
const KEY_SCROLL_LINES: usize = 10;
/// Lines scrolled per mouse wheel notch.
const MOUSE_SCROLL_LINES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChordMode {
    Normal,
    Awaiting,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppState {
    AddressBook,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum InputMode {
    #[serde(rename = "line")]
    LineBuffered,
    #[serde(rename = "character")]
    Character,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PopupField {
    Name,
    Host,
    Port,
    Protocol,
    Username,
    TerminalType,
    InputMode,
}

/// Sentinel shown at the head of the form popup's terminal-type cycle. When
/// the user leaves it on this option, the entry stores `terminal_type: None`,
/// which tells `App::connect` to fall back to whatever Settings says.
const FORM_TERMINAL_TYPE_DEFAULT: &str = "(default)";

/// Sentinel for the form popup's per-entry input-mode cycle. `None` on the
/// entry means "auto" (SSH → character; telnet → settings, then WILL ECHO).
const FORM_INPUT_MODE_DEFAULT: &str = "(default)";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FormMode {
    Add,
    Edit,
}

pub enum Popup {
    Form(FormPopup),
    EditSettings(EditSettingsPopup),
    DeleteConfirm,
    Password(String),
    HostKeyTrust(HostKeyTrustPopup),
    ChordHelp,
}

pub struct HostKeyTrustPopup {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
}

pub struct FormPopup {
    pub mode: FormMode,
    pub focused: PopupField,
    pub name: String,
    pub host: String,
    pub port_str: String,
    pub protocol: Protocol,
    pub username: String,
    pub terminal_type_options: Vec<String>,
    pub terminal_type_idx: usize,
    /// Cycle index into `(default)` / `line` / `character`.
    pub input_mode_idx: usize,
}

impl FormPopup {
    const INPUT_MODE_OPTIONS: &[&'static str] = &[FORM_INPUT_MODE_DEFAULT, "line", "character"];

    pub fn new_add() -> Self {
        Self {
            mode: FormMode::Add,
            focused: PopupField::Name,
            name: String::new(),
            host: String::new(),
            port_str: "23".into(),
            protocol: Protocol::Telnet,
            username: String::new(),
            terminal_type_options: form_terminal_type_options(None),
            terminal_type_idx: 0,
            input_mode_idx: 0,
        }
    }

    pub fn new_edit(entry: &AddressBookEntry) -> Self {
        let options = form_terminal_type_options(entry.terminal_type.as_deref());
        let terminal_type_idx = match &entry.terminal_type {
            None => 0,
            Some(s) => options.iter().position(|o| o == s).unwrap_or(0),
        };
        let input_mode_idx = match entry.default_input_mode {
            None => 0,
            Some(InputMode::LineBuffered) => 1,
            Some(InputMode::Character) => 2,
        };
        Self {
            mode: FormMode::Edit,
            focused: PopupField::Name,
            name: entry.name.clone(),
            host: entry.host.clone(),
            port_str: entry.port.to_string(),
            protocol: entry.protocol,
            username: entry.username.clone().unwrap_or_default(),
            terminal_type_options: options,
            terminal_type_idx,
            input_mode_idx,
        }
    }

    pub fn next_field(&mut self) {
        self.focused = match self.focused {
            PopupField::Name => PopupField::Host,
            PopupField::Host => PopupField::Port,
            PopupField::Port => PopupField::Protocol,
            PopupField::Protocol => PopupField::Username,
            PopupField::Username => PopupField::TerminalType,
            PopupField::TerminalType => PopupField::InputMode,
            PopupField::InputMode => PopupField::Name,
        };
    }

    pub fn prev_field(&mut self) {
        self.focused = match self.focused {
            PopupField::Name => PopupField::InputMode,
            PopupField::Host => PopupField::Name,
            PopupField::Port => PopupField::Host,
            PopupField::Protocol => PopupField::Port,
            PopupField::Username => PopupField::Protocol,
            PopupField::TerminalType => PopupField::Username,
            PopupField::InputMode => PopupField::TerminalType,
        };
    }

    pub fn cycle_terminal_type(&mut self) {
        if self.terminal_type_options.is_empty() {
            return;
        }
        self.terminal_type_idx = (self.terminal_type_idx + 1) % self.terminal_type_options.len();
    }

    pub fn terminal_type_label(&self) -> &str {
        &self.terminal_type_options[self.terminal_type_idx]
    }

    /// Returns `None` when the popup is on the "(default)" sentinel, otherwise
    /// the configured override string. Callers store this directly on the
    /// `AddressBookEntry`, where `None` means "fall back to Settings".
    pub fn terminal_type_override(&self) -> Option<String> {
        let v = self.terminal_type_label();
        if v == FORM_TERMINAL_TYPE_DEFAULT {
            None
        } else {
            Some(v.to_string())
        }
    }

    pub fn cycle_input_mode(&mut self) {
        self.input_mode_idx = (self.input_mode_idx + 1) % Self::INPUT_MODE_OPTIONS.len();
    }

    pub fn input_mode_label(&self) -> &str {
        Self::INPUT_MODE_OPTIONS[self.input_mode_idx]
    }

    pub fn input_mode_override(&self) -> Option<InputMode> {
        match self.input_mode_label() {
            "line" => Some(InputMode::LineBuffered),
            "character" => Some(InputMode::Character),
            _ => None,
        }
    }

    pub fn type_char(&mut self, c: char) {
        if let Some(field) = self.text_field_mut() {
            field.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.text_field_mut() {
            field.pop();
        }
    }

    pub fn toggle_protocol(&mut self) {
        self.protocol = match self.protocol {
            Protocol::Telnet => {
                if self.port_str == "23" {
                    self.port_str = "22".into();
                } else if self.port_str == "2323" {
                    // modernbbs telnet → SSH preset
                    self.port_str = "2222".into();
                }
                Protocol::Ssh
            }
            Protocol::Ssh => {
                if self.port_str == "22" {
                    self.port_str = "23".into();
                } else if self.port_str == "2222" {
                    // modernbbs SSH → telnet preset
                    self.port_str = "2323".into();
                }
                Protocol::Telnet
            }
        };
    }

    pub fn to_entry(&self) -> Option<AddressBookEntry> {
        let port: u16 = self.port_str.parse().ok()?;
        if self.name.is_empty() || self.host.is_empty() {
            return None;
        }
        Some(AddressBookEntry {
            name: self.name.clone(),
            host: self.host.clone(),
            port,
            protocol: self.protocol,
            username: if self.username.is_empty() {
                None
            } else {
                Some(self.username.clone())
            },
            terminal_type: self.terminal_type_override(),
            default_input_mode: self.input_mode_override(),
        })
    }

    fn text_field_mut(&mut self) -> Option<&mut String> {
        match self.focused {
            PopupField::Name => Some(&mut self.name),
            PopupField::Host => Some(&mut self.host),
            PopupField::Port => Some(&mut self.port_str),
            PopupField::Username => Some(&mut self.username),
            PopupField::Protocol | PopupField::TerminalType | PopupField::InputMode => None,
        }
    }
}

/// Builds the cycle list shown in the form popup's terminal-type field. The
/// "(default)" sentinel is always first; if `current` is set to a value not in
/// the standard list, it is preserved as a second entry so a hand-edited
/// `address_book.toml` round-trips through the popup without losing its
/// custom value.
fn form_terminal_type_options(current: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = std::iter::once(FORM_TERMINAL_TYPE_DEFAULT.to_string())
        .chain(STANDARD_TERMINAL_TYPES.iter().map(|s| (*s).to_string()))
        .collect();
    if let Some(c) = current
        && !c.is_empty()
        && !out.iter().any(|o| o == c)
    {
        out.insert(1, c.to_string());
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingsField {
    Scrollback,
    Mode,
    TerminalType,
}

/// Curated terminal-type values offered in the settings popup. Users with
/// unusual needs (e.g. `rxvt-256color`) can still hand-edit `settings.toml`,
/// or — if their existing value isn't in this list — `from_settings` will
/// preserve it as a one-off entry at the head of the cycle.
const STANDARD_TERMINAL_TYPES: &[&str] =
    &["xterm-256color", "xterm", "ansi", "vt100", "vt220", "dumb"];

pub struct EditSettingsPopup {
    pub focused: SettingsField,
    pub scrollback_input: String,
    pub mode: InputMode,
    pub terminal_type_options: Vec<String>,
    pub terminal_type_idx: usize,
    pub error: Option<String>,
}

impl EditSettingsPopup {
    pub fn from_settings(s: &config::settings::Settings) -> Self {
        let mut options: Vec<String> = STANDARD_TERMINAL_TYPES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let terminal_type_idx = if s.terminal_type.is_empty() {
            0
        } else if let Some(i) = options.iter().position(|o| o == &s.terminal_type) {
            i
        } else {
            options.insert(0, s.terminal_type.clone());
            0
        };
        Self {
            focused: SettingsField::Scrollback,
            scrollback_input: s.scrollback_lines.to_string(),
            mode: s.default_input_mode,
            terminal_type_options: options,
            terminal_type_idx,
            error: None,
        }
    }

    pub fn next_field(&mut self) {
        self.focused = match self.focused {
            SettingsField::Scrollback => SettingsField::Mode,
            SettingsField::Mode => SettingsField::TerminalType,
            SettingsField::TerminalType => SettingsField::Scrollback,
        };
    }

    pub fn prev_field(&mut self) {
        self.focused = match self.focused {
            SettingsField::Scrollback => SettingsField::TerminalType,
            SettingsField::Mode => SettingsField::Scrollback,
            SettingsField::TerminalType => SettingsField::Mode,
        };
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            InputMode::LineBuffered => InputMode::Character,
            InputMode::Character => InputMode::LineBuffered,
        };
    }

    pub fn cycle_terminal_type(&mut self) {
        if self.terminal_type_options.is_empty() {
            return;
        }
        self.terminal_type_idx = (self.terminal_type_idx + 1) % self.terminal_type_options.len();
    }

    pub fn terminal_type_value(&self) -> &str {
        &self.terminal_type_options[self.terminal_type_idx]
    }

    /// Validate and produce a Settings on success, or set `self.error` and
    /// return None on failure. Caller owns persistence + closing the popup.
    pub fn validate(&mut self) -> Option<config::settings::Settings> {
        let scrollback = match self.scrollback_input.trim().parse::<usize>() {
            Ok(n) if n <= 100_000 => n,
            _ => {
                self.error = Some("Scrollback must be a number between 0 and 100,000.".into());
                return None;
            }
        };
        let terminal_type = self.terminal_type_value().to_string();
        self.error = None;
        Some(config::settings::Settings {
            scrollback_lines: scrollback,
            default_input_mode: self.mode,
            terminal_type,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Telnet,
    Ssh,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Telnet => write!(f, "Telnet"),
            Protocol::Ssh => write!(f, "SSH"),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct AddressBookEntry {
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default = "default_protocol")]
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Per-entry override for the telnet/SSH terminal type. `None` means
    /// "use whatever Settings says." Existing TOML files without the field
    /// load as `None` thanks to `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_type: Option<String>,
    /// Per-entry default input mode. `None` means auto: SSH starts in
    /// character mode; telnet uses Settings and may switch to character when
    /// the server negotiates WILL ECHO.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_input_mode: Option<InputMode>,
}

fn default_protocol() -> Protocol {
    Protocol::Telnet
}

pub struct App {
    pub state: AppState,
    pub input_mode: InputMode,
    pub popup: Option<Popup>,
    pub entries: Vec<AddressBookEntry>,
    pub selected: usize,
    pub connected_entry: Option<usize>, // index of actively connected entry
    pub emulator: TerminalEmulator,
    pub input: String,
    pub status_message: String,
    pub settings: config::settings::Settings,
    event_tx: mpsc::Sender<AppEvent>,
    connection_tx: Option<mpsc::Sender<ConnectionCommand>>,
    telnet_flags: Option<Arc<TelnetFlags>>,
    history: Vec<String>,
    history_index: Option<usize>, // None = typing new input, Some(i) = browsing history
    history_draft: String,        // saves in-progress input when browsing history
    char_input_len: usize,        // tracks typed chars on current line in CHAR mode
    connection_id: u64,
    connection_handle: Option<tokio::task::JoinHandle<()>>,
    /// Shared cancel for every per-connection task (reader + writer + handshake).
    connection_cancel: Option<CancellationToken>,
    password_reply: Option<tokio::sync::oneshot::Sender<String>>,
    host_key_reply: Option<tokio::sync::oneshot::Sender<bool>>,
    pub capture: Option<config::capture::CaptureFile>,
    ansi_query_scanner: AnsiQueryScanner,
    chord: ChordMode,
    shown_chord_hint: bool,
    /// True once the user has Tab-toggled input mode this session. Suppresses
    /// telnet WILL-ECHO auto-switch so we don't fight the user's choice.
    input_mode_user_set: bool,
    quit: bool,
    width: u16,
    height: u16,
}

impl App {
    pub fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        let loaded = config::address_book::load();
        let kh = config::known_hosts::load();
        let s = config::settings::load();
        let status_message = [loaded.warning, kh.warning, s.warning]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        let settings = s.settings;
        let scrollback = settings.scrollback_lines;
        let initial_mode = settings.default_input_mode;
        Self {
            state: AppState::AddressBook,
            entries: loaded.entries,
            input_mode: initial_mode,
            popup: None,
            selected: 0,
            connected_entry: None,
            emulator: TerminalEmulator::new(24, 80, scrollback),
            input: String::new(),
            status_message,
            settings,
            event_tx,
            connection_tx: None,
            telnet_flags: None,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            char_input_len: 0,
            connection_id: 0,
            connection_handle: None,
            connection_cancel: None,
            password_reply: None,
            host_key_reply: None,
            capture: None,
            ansi_query_scanner: AnsiQueryScanner::new(),
            chord: ChordMode::Normal,
            shown_chord_hint: false,
            input_mode_user_set: false,
            quit: false,
            width: 80,
            height: 24,
        }
    }

    /// Build an App with fixed entries/settings and no disk I/O. Used by unit
    /// tests that drive `handle_app_event` / key handling without a TTY.
    #[cfg(test)]
    fn new_for_test(
        event_tx: mpsc::Sender<AppEvent>,
        entries: Vec<AddressBookEntry>,
        settings: config::settings::Settings,
    ) -> Self {
        let scrollback = settings.scrollback_lines;
        let initial_mode = settings.default_input_mode;
        Self {
            state: AppState::AddressBook,
            entries,
            input_mode: initial_mode,
            popup: None,
            selected: 0,
            connected_entry: None,
            emulator: TerminalEmulator::new(24, 80, scrollback),
            input: String::new(),
            status_message: String::new(),
            settings,
            event_tx,
            connection_tx: None,
            telnet_flags: None,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            char_input_len: 0,
            connection_id: 0,
            connection_handle: None,
            connection_cancel: None,
            password_reply: None,
            host_key_reply: None,
            capture: None,
            ansi_query_scanner: AnsiQueryScanner::new(),
            chord: ChordMode::Normal,
            shown_chord_hint: true, // suppress one-shot hint noise in tests
            input_mode_user_set: false,
            quit: false,
            width: 80,
            height: 24,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    fn save_entries(&mut self) {
        if let Err(e) = config::address_book::save(&self.entries) {
            self.status_message = format!("Failed to save: {}", e);
        }
    }

    /// Returns true if we should echo typed characters locally.
    /// SSH always handles echo via PTY — no local echo needed.
    /// For telnet: when server negotiates WILL ECHO, it handles echoing.
    fn needs_local_echo(&self) -> bool {
        match &self.telnet_flags {
            Some(flags) => !flags.server_echo.load(Ordering::Relaxed),
            None => false, // SSH or no telnet flags = server handles echo
        }
    }

    /// Viewport size matching `ui/terminal_view` layout:
    /// character mode = full height minus status bar; line mode also
    /// subtracts the 3-row input bar.
    pub fn terminal_viewport(&self) -> (u16, u16) {
        let cols = self.width.max(1);
        let chrome: u16 = match self.input_mode {
            InputMode::Character => 1,    // status bar
            InputMode::LineBuffered => 4, // status + input bar
        };
        let rows = self.height.saturating_sub(chrome).max(1);
        (cols, rows)
    }

    /// Resize the local emulator and notify the peer (NAWS / SSH window_change).
    async fn apply_viewport_to_connection(&mut self) {
        let (cols, rows) = self.terminal_viewport();
        self.emulator.resize(rows, cols);
        if let Some(tx) = &self.connection_tx {
            let _ = tx.send(ConnectionCommand::Resize(cols, rows)).await;
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    pub async fn handle_crossterm_event(&mut self, event: CrosstermEvent) -> Result<()> {
        match event {
            // Only react to Press/Repeat. If the terminal ever delivers Release
            // events (kitty keyboard protocol etc.), they would otherwise
            // double-fire handlers and silently consume chord state.
            CrosstermEvent::Key(key) if key.kind == KeyEventKind::Release => {}
            CrosstermEvent::Key(key) => self.handle_key(key).await?,
            CrosstermEvent::Mouse(mouse) => self.handle_mouse(mouse),
            CrosstermEvent::Resize(w, h) => {
                self.resize(w, h);
                self.apply_viewport_to_connection().await;
            }
            CrosstermEvent::Paste(text) => self.handle_paste(text).await?,
            _ => {}
        }
        Ok(())
    }

    /// Bracketed-paste payload from the terminal. Routes into the active
    /// text field, password popup, or the live connection.
    async fn handle_paste(&mut self, text: String) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        // Popups first — same priority as key handling.
        match self.popup.as_mut() {
            Some(Popup::Password(pw)) => {
                // Strip newlines; password fields are single-line.
                for c in text.chars() {
                    if c != '\n' && c != '\r' {
                        pw.push(c);
                    }
                }
                return Ok(());
            }
            Some(Popup::Form(form)) => {
                for c in text.chars() {
                    if c != '\n' && c != '\r' {
                        form.type_char(c);
                    }
                }
                return Ok(());
            }
            Some(Popup::EditSettings(p)) if p.focused == SettingsField::Scrollback => {
                for c in text.chars() {
                    if c.is_ascii_digit() {
                        p.scrollback_input.push(c);
                    }
                }
                return Ok(());
            }
            Some(_) => return Ok(()), // other popups ignore paste
            None => {}
        }

        match self.state {
            AppState::Connected => match self.input_mode {
                InputMode::LineBuffered => {
                    for c in text.chars() {
                        if c == '\n' || c == '\r' {
                            // Ignore embedded newlines in line mode — user
                            // hits Enter explicitly to send.
                            continue;
                        }
                        self.input.push(c);
                    }
                    self.history_index = None;
                }
                InputMode::Character => {
                    if let Some(tx) = &self.connection_tx {
                        // Normalize line endings to CR LF for remote hosts.
                        let mut bytes = Vec::with_capacity(text.len());
                        let mut chars = text.chars().peekable();
                        while let Some(c) = chars.next() {
                            if c == '\r' {
                                bytes.extend_from_slice(b"\r\n");
                                if chars.peek() == Some(&'\n') {
                                    chars.next();
                                }
                            } else if c == '\n' {
                                bytes.extend_from_slice(b"\r\n");
                            } else {
                                let mut buf = [0u8; 4];
                                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                        if !bytes.is_empty() {
                            if self.needs_local_echo() {
                                self.emulator.process(&bytes);
                            }
                            let _ = tx.send(ConnectionCommand::SendRaw(bytes)).await;
                        }
                    }
                }
            },
            AppState::AddressBook | AppState::Connecting => {}
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.state != AppState::Connected {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.emulator.scroll_up(MOUSE_SCROLL_LINES),
            MouseEventKind::ScrollDown => self.emulator.scroll_down(MOUSE_SCROLL_LINES),
            _ => {}
        }
    }

    fn handle_key_popup(&mut self, key: KeyEvent) {
        match self.popup.as_mut() {
            None => {}
            Some(Popup::Form(_)) => self.handle_key_form_popup(key),
            Some(Popup::EditSettings(_)) => self.handle_key_edit_settings_popup(key),
            Some(Popup::DeleteConfirm) => self.handle_key_delete_popup(key),
            Some(Popup::Password(_)) => self.handle_key_password_popup(key),
            Some(Popup::HostKeyTrust(_)) => self.handle_key_host_key_trust_popup(key),
            Some(Popup::ChordHelp) => self.handle_key_chord_help_popup(key),
        }
    }

    fn handle_key_chord_help_popup(&mut self, _key: KeyEvent) {
        self.popup = None;
    }

    fn handle_key_host_key_trust_popup(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(reply) = self.host_key_reply.take() {
                    let _ = reply.send(true);
                }
                self.popup = None;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                if let Some(reply) = self.host_key_reply.take() {
                    let _ = reply.send(false);
                }
                self.popup = None;
            }
            _ => {} // ignore everything else
        }
    }

    fn handle_key_password_popup(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let popup = self.popup.take();
                if let (Some(Popup::Password(pw)), Some(reply)) =
                    (popup, self.password_reply.take())
                {
                    let _ = reply.send(pw);
                }
            }
            KeyCode::Esc => {
                self.popup = None;
                // Drop the reply sender — SSH task will see the channel closed
                self.password_reply = None;
            }
            KeyCode::Char(c) => {
                if let Some(Popup::Password(pw)) = self.popup.as_mut() {
                    pw.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(Popup::Password(pw)) = self.popup.as_mut() {
                    pw.pop();
                }
            }
            _ => {}
        }
    }

    fn handle_key_delete_popup(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') if !self.entries.is_empty() => {
                let deleted = self.selected;
                let name = self.entries[deleted].name.clone();

                // `connected_entry` is a positional index — adjust or clear so
                // the green marker / resume / capture stay attached to the
                // live session (or drop it if that entry is gone).
                if let Some(conn) = self.connected_entry {
                    if conn == deleted {
                        self.teardown_connection_sync();
                        self.connected_entry = None;
                        self.telnet_flags = None;
                        // Bump id so any in-flight Connected/NetworkData is stale.
                        self.connection_id = self.connection_id.wrapping_add(1);
                    } else if deleted < conn {
                        self.connected_entry = Some(conn - 1);
                    }
                }

                self.entries.remove(deleted);
                if self.selected >= self.entries.len() && self.selected > 0 {
                    self.selected -= 1;
                }
                self.status_message = format!("Deleted '{}'", name);
                self.save_entries();
            }
            _ => {}
        }
        self.popup = None;
    }

    fn handle_key_form_popup(&mut self, key: KeyEvent) {
        let Some(Popup::Form(form)) = self.popup.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.popup = None;
            }
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.prev_field(),
            KeyCode::Char(' ') if form.focused == PopupField::Protocol => form.toggle_protocol(),
            KeyCode::Char(' ') if form.focused == PopupField::TerminalType => {
                form.cycle_terminal_type()
            }
            KeyCode::Char(' ') if form.focused == PopupField::InputMode => form.cycle_input_mode(),
            KeyCode::Char(c) => form.type_char(c),
            KeyCode::Backspace => form.backspace(),
            KeyCode::Enter => {
                let Some(Popup::Form(form)) = self.popup.as_ref() else {
                    return;
                };
                let Some(entry) = form.to_entry() else {
                    self.status_message =
                        "Invalid entry (name, host required; port must be a number)".into();
                    return;
                };
                let mode = form.mode;
                self.popup = None;
                match mode {
                    FormMode::Add => {
                        self.status_message = format!("Added '{}'", entry.name);
                        self.entries.push(entry);
                        self.selected = self.entries.len() - 1;
                        self.save_entries();
                    }
                    FormMode::Edit => {
                        self.status_message = format!("Updated '{}'", entry.name);
                        self.entries[self.selected] = entry;
                        self.save_entries();
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_key_edit_settings_popup(&mut self, key: KeyEvent) {
        let Some(Popup::EditSettings(p)) = self.popup.as_mut() else {
            return;
        };

        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.popup = None;
            }
            (KeyCode::Tab, m) if m.contains(KeyModifiers::SHIFT) => p.prev_field(),
            (KeyCode::BackTab, _) => p.prev_field(),
            (KeyCode::Tab, _) => p.next_field(),
            (KeyCode::Char(' '), _) if p.focused == SettingsField::Mode => p.toggle_mode(),
            (KeyCode::Char(' '), _) if p.focused == SettingsField::TerminalType => {
                p.cycle_terminal_type()
            }
            (KeyCode::Backspace, _) => match p.focused {
                SettingsField::Scrollback => {
                    p.scrollback_input.pop();
                }
                SettingsField::Mode | SettingsField::TerminalType => {}
            },
            (KeyCode::Char(c), _) => match p.focused {
                SettingsField::Scrollback => p.scrollback_input.push(c),
                SettingsField::Mode | SettingsField::TerminalType => {}
            },
            (KeyCode::Enter, _) => {
                if let Some(new_settings) = p.validate() {
                    if let Err(e) = config::settings::save(&new_settings) {
                        p.error = Some(format!("Could not save settings: {}", e));
                    } else {
                        self.settings = new_settings;
                        self.popup = None;
                    }
                }
            }
            _ => {}
        }
    }

    pub async fn handle_app_event(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Connected {
                id,
                cmd_tx,
                telnet_flags,
            } => {
                if id != self.connection_id {
                    return Ok(()); // stale connection, ignore
                }
                self.state = AppState::Connected;
                self.connection_tx = Some(cmd_tx);
                self.telnet_flags = telnet_flags;
                self.ansi_query_scanner = AnsiQueryScanner::new();
                self.connected_entry = Some(self.selected);
                self.status_message = format!(
                    "Connected to {}",
                    self.entries
                        .get(self.selected)
                        .map(|e| e.name.as_str())
                        .unwrap_or("unknown")
                );
                if !self.shown_chord_hint {
                    self.shown_chord_hint = true;
                    self.status_message
                        .push_str(" — Press Ctrl+] ? for commands.");
                }
            }
            AppEvent::NetworkData { id, data } => {
                if id != self.connection_id {
                    return Ok(());
                }
                // Detect ANSI query sequences (CPR / DSR / DA) with byte
                // offsets so CPR can sample the cursor *at* each query, not
                // after later cursor-moving output in the same read.
                let queries = self.ansi_query_scanner.scan(&data);
                // Process data even when viewing address book (session suspended),
                // segmented at each query so cursor position is correct for CPR.
                let mut pos = 0usize;
                for detected in &queries {
                    let end = detected.end.min(data.len());
                    if end > pos {
                        self.emulator.process(&data[pos..end]);
                        pos = end;
                    }
                    if let Some(tx) = &self.connection_tx {
                        let response = match detected.query {
                            AnsiQuery::CursorPositionReport => {
                                let (row, col) = self.emulator.cursor_position();
                                cpr_response(row, col)
                            }
                            AnsiQuery::DeviceStatusOk => dsr_ok_response(),
                            AnsiQuery::PrimaryDeviceAttributes => da_response(),
                        };
                        let _ = tx.send(ConnectionCommand::SendRaw(response)).await;
                    }
                }
                if pos < data.len() {
                    self.emulator.process(&data[pos..]);
                }

                // Telnet WILL ECHO → character mode (unless the user Tab-toggled).
                self.maybe_auto_switch_input_mode().await;

                // Tee to capture file if active. Fail loud: any write error
                // closes the file, flips the indicator off, and flashes a
                // failure message that includes bytes saved + path.
                if let Some(cap) = self.capture.as_mut()
                    && let Err(e) = cap.write(&data)
                {
                    let cap = self.capture.take().expect("just borrowed");
                    let path = cap.path().to_path_buf();
                    let bytes = cap.bytes_written();
                    drop(cap);
                    self.status_message = format!(
                        "Capture failed: {}; saved {} bytes → {}",
                        e,
                        bytes,
                        path.display()
                    );
                }
            }
            AppEvent::PasswordNeeded { id, reply } => {
                if id != self.connection_id {
                    return Ok(());
                }
                self.password_reply = Some(reply);
                self.popup = Some(Popup::Password(String::new()));
                self.status_message = "SSH password required".into();
            }
            AppEvent::Disconnected { id, reason } => {
                if id != self.connection_id {
                    return Ok(());
                }
                self.state = AppState::AddressBook;
                self.connection_tx = None;
                self.telnet_flags = None;
                self.connection_handle = None;
                self.connection_cancel = None;
                self.connected_entry = None;
                self.status_message = match reason {
                    Some(err) => format!("Disconnected: {}", err),
                    None => "Disconnected".into(),
                };
                // Close any open capture and overwrite status_message with the
                // capture summary — the user's most useful info at disconnect
                // is "where did the file go?", not the connection reason.
                if let Some(cap) = self.capture.take() {
                    let path = cap.path().to_path_buf();
                    let bytes = cap.bytes_written();
                    drop(cap);
                    self.status_message = format!(
                        "Capture stopped (disconnected): saved {} bytes → {}",
                        bytes,
                        path.display()
                    );
                }
            }
            AppEvent::HostKeyTrustNeeded {
                id,
                host,
                port,
                key_type,
                fingerprint,
                reply,
            } => {
                if id != self.connection_id {
                    return Ok(());
                }
                self.host_key_reply = Some(reply);
                self.popup = Some(Popup::HostKeyTrust(HostKeyTrustPopup {
                    host: host.clone(),
                    port,
                    key_type,
                    fingerprint,
                }));
                self.status_message = format!("Verify host key for {}:{}", host, port);
            }
            AppEvent::HostKeyMismatch {
                id,
                host,
                port,
                key_type,
                stored_fingerprint,
                received_fingerprint,
                file_path,
            } => {
                if id != self.connection_id {
                    return Ok(());
                }
                self.state = AppState::AddressBook;
                self.status_message = format!(
                    "HOST KEY MISMATCH for {host}:{port} ({key_type})\n  \
                     stored:   {stored_fingerprint}\n  \
                     received: {received_fingerprint}\n\
                     This could be a man-in-the-middle attack, or the server's key\n\
                     was rotated. To accept the new key, edit:\n  \
                     {path}\n\
                     and remove the [[hosts]] entry with key_type = \"{key_type}\".",
                    path = file_path.display(),
                );
                // Bump the connection id so the russh-abort Disconnected event that
                // follows is filtered out and does not clobber this banner.
                self.connection_id = self.connection_id.wrapping_add(1);
            }
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Ctrl+C quits everywhere except character mode while connected —
        // there ^C must reach the remote (SIGINT / break). Quit from a
        // char-mode session with Ctrl+] q instead.
        let in_char_session = self.state == AppState::Connected
            && self.input_mode == InputMode::Character
            && self.popup.is_none()
            && self.chord == ChordMode::Normal;
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
            && !in_char_session
        {
            self.disconnect().await;
            self.quit = true;
            return Ok(());
        }

        // Popup intercepts all keys when open
        if self.popup.is_some() {
            self.handle_key_popup(key);
            return Ok(());
        }

        match self.state {
            AppState::AddressBook => self.handle_key_address_book(key).await?,
            AppState::Connecting => self.handle_key_connecting(key).await,
            AppState::Connected => self.handle_key_connected(key).await?,
        }
        Ok(())
    }

    /// Toggle session capture on/off. Called from the chord handler when the
    /// user presses `Ctrl+] l`. Spec rules:
    ///   * If capture is on, close the file and flash bytes-saved + path.
    ///   * If capture is off, open a fresh file for the currently-connected
    ///     entry. Flash success or failure.
    fn toggle_capture(&mut self) {
        if let Some(cap) = self.capture.take() {
            // Toggle off: read path/bytes before drop closes the file.
            let path = cap.path().to_path_buf();
            let bytes = cap.bytes_written();
            drop(cap);
            self.status_message = format!(
                "Capture stopped: saved {} bytes → {}",
                bytes,
                path.display()
            );
            return;
        }

        // Toggle on: resolve the currently connected entry.
        let Some(idx) = self.connected_entry else {
            // Defensive guard — chord is only active in connected mode, but
            // we still bail loudly rather than panic if state diverges.
            self.status_message = "Capture: no active connection".into();
            return;
        };
        let Some(entry) = self.entries.get(idx) else {
            self.status_message = "Capture: connected entry vanished".into();
            return;
        };
        let entry_name = entry.name.clone();
        let host = entry.host.clone();
        let port = entry.port;

        match config::capture::open(&entry_name, &host, port) {
            Ok(file) => {
                let path = file.path().display().to_string();
                self.capture = Some(file);
                self.status_message = format!("Capture started → {}", path);
            }
            Err(e) => {
                self.status_message = format!("Capture failed: {}", e);
            }
        }
    }

    async fn handle_key_connecting(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancel_connection().await;
            // Bump connection_id so a Connected event already queued for the
            // aborted attempt is treated as stale (otherwise we flip back to
            // Connected with a dead cmd_tx).
            self.connection_id = self.connection_id.wrapping_add(1);
            self.state = AppState::AddressBook;
            self.status_message = "Connection cancelled".into();
        }
    }

    async fn handle_key_address_book(&mut self, key: KeyEvent) -> Result<()> {
        // Ctrl+D: disconnect active session from address book
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            if self.connected_entry.is_some() {
                self.disconnect().await;
                self.connected_entry = None;
                self.status_message = "Disconnected".into();
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                self.disconnect().await;
                self.connected_entry = None;
                self.quit = true;
            }
            KeyCode::Up | KeyCode::Char('k') if !self.entries.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.entries.len() - 1
                } else {
                    self.selected - 1
                };
            }
            KeyCode::Down | KeyCode::Char('j') if !self.entries.is_empty() => {
                self.selected = (self.selected + 1) % self.entries.len();
            }
            KeyCode::Enter => {
                // If already connected to this entry, resume
                if self.connected_entry == Some(self.selected) && self.connection_tx.is_some() {
                    self.state = AppState::Connected;
                    self.status_message = format!(
                        "Connected to {}",
                        self.entries
                            .get(self.selected)
                            .map(|e| e.name.as_str())
                            .unwrap_or("unknown")
                    );
                } else {
                    self.connect().await?;
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.popup = Some(Popup::Form(FormPopup::new_add()));
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                if let Some(entry) = self.entries.get(self.selected) {
                    self.popup = Some(Popup::Form(FormPopup::new_edit(entry)));
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') if !self.entries.is_empty() => {
                self.popup = Some(Popup::DeleteConfirm);
            }
            KeyCode::Char('S') | KeyCode::Char('s') => {
                self.popup = Some(Popup::EditSettings(EditSettingsPopup::from_settings(
                    &self.settings,
                )));
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_key_connected(&mut self, key: KeyEvent) -> Result<()> {
        // Chord dispatch — if we're awaiting a chord command, this key is the
        // chord, not a normal key. Always reset state to Normal first so that
        // a panicking dispatch can't wedge us in Awaiting.
        if self.chord == ChordMode::Awaiting {
            self.chord = ChordMode::Normal;
            match key.code {
                KeyCode::Char('l') | KeyCode::Char('L') => self.toggle_capture(),
                KeyCode::Char('?') => self.popup = Some(Popup::ChordHelp),
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.disconnect().await;
                    self.connected_entry = None;
                    self.quit = true;
                }
                // Esc cancels the chord with no flash; any other key is a
                // mistype that we silently swallow rather than forwarding to
                // the network (the user clearly intended a chord).
                _ => {}
            }
            return Ok(());
        }

        // Chord trigger — Ctrl+] enters Awaiting state.
        // Crossterm 0.28 in basic (non-kitty) mode reports raw 0x1D as
        // KeyCode::Char('5') + CONTROL (legacy ASCII mapping); kitty mode
        // reports it as KeyCode::Char(']'). Accept both.
        // Always reserved (never forwarded) so local commands stay available.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
        {
            self.chord = ChordMode::Awaiting;
            self.status_message = "Ctrl+] (waiting for command — ? for help)".into();
            return Ok(());
        }

        // Esc: suspend session and return to address book (stay connected)
        if key.code == KeyCode::Esc {
            self.state = AppState::AddressBook;
            self.status_message = format!(
                "Session suspended — {} (Enter to resume, Ctrl+D to disconnect)",
                self.entries
                    .get(self.selected)
                    .map(|e| e.name.as_str())
                    .unwrap_or("unknown")
            );
            return Ok(());
        }

        // Ctrl+D: disconnect in line mode only. In character mode ^D is EOT
        // and must reach the remote (EOF / logout prompts).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('d')
            && self.input_mode == InputMode::LineBuffered
        {
            self.disconnect().await;
            self.connected_entry = None;
            self.state = AppState::AddressBook;
            self.status_message = "Disconnected".into();
            return Ok(());
        }

        // Tab toggles input mode — viewport chrome changes, so tell the peer.
        if key.code == KeyCode::Tab && key.modifiers.is_empty() {
            self.input_mode = match self.input_mode {
                InputMode::LineBuffered => InputMode::Character,
                InputMode::Character => InputMode::LineBuffered,
            };
            self.input_mode_user_set = true;
            self.apply_viewport_to_connection().await;
            return Ok(());
        }

        // Shift+PageUp/Down always scrolls locally, regardless of mode
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            match key.code {
                KeyCode::PageUp => {
                    self.emulator.scroll_up(KEY_SCROLL_LINES);
                    return Ok(());
                }
                KeyCode::PageDown => {
                    self.emulator.scroll_down(KEY_SCROLL_LINES);
                    return Ok(());
                }
                _ => {}
            }
        }

        match self.input_mode {
            InputMode::LineBuffered => self.handle_key_line_buffered(key).await?,
            InputMode::Character => self.handle_key_character(key).await?,
        }
        Ok(())
    }

    async fn handle_key_line_buffered(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Enter => {
                if let Some(tx) = &self.connection_tx {
                    let text = format!("{}\r\n", self.input);
                    if self.needs_local_echo() {
                        self.emulator
                            .process(format!("{}\r\n", self.input).as_bytes());
                    }
                    // Save non-empty input to history
                    if !self.input.is_empty() {
                        // Avoid consecutive duplicates
                        if self.history.last().map(|s| s.as_str()) != Some(&self.input) {
                            self.history.push(self.input.clone());
                        }
                    }
                    self.history_index = None;
                    let _ = tx.send(ConnectionCommand::SendText(text)).await;
                    self.input.clear();
                }
            }
            KeyCode::Up if !self.history.is_empty() => {
                match self.history_index {
                    None => {
                        // Start browsing: save current input, show last history entry
                        self.history_draft = self.input.clone();
                        self.history_index = Some(self.history.len() - 1);
                        self.input = self.history[self.history.len() - 1].clone();
                    }
                    Some(i) if i > 0 => {
                        self.history_index = Some(i - 1);
                        self.input = self.history[i - 1].clone();
                    }
                    _ => {} // already at oldest
                }
            }
            KeyCode::Down => {
                if let Some(i) = self.history_index {
                    if i + 1 < self.history.len() {
                        self.history_index = Some(i + 1);
                        self.input = self.history[i + 1].clone();
                    } else {
                        // Past the end: restore draft
                        self.history_index = None;
                        self.input = self.history_draft.clone();
                        self.history_draft.clear();
                    }
                }
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.history_index = None; // typing breaks out of history browse
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.history_index = None;
            }
            KeyCode::PageUp => {
                self.emulator.scroll_up(KEY_SCROLL_LINES);
            }
            KeyCode::PageDown => {
                self.emulator.scroll_down(KEY_SCROLL_LINES);
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_key_character(&mut self, key: KeyEvent) -> Result<()> {
        let Some(tx) = &self.connection_tx else {
            return Ok(());
        };

        // Build bytes to send and optional local echo bytes
        let (send, echo): (Option<Vec<u8>>, Option<Vec<u8>>) = match key.code {
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    // Map Ctrl+A..Ctrl+Z → 0x01..0x1A (incl. ^C=3, ^D=4).
                    let lower = c.to_ascii_lowercase();
                    if lower.is_ascii_lowercase() {
                        let ctrl = (lower as u8) - b'a' + 1;
                        (Some(vec![ctrl]), None)
                    } else {
                        (None, None)
                    }
                } else {
                    self.char_input_len += 1;
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf);
                    let bytes = s.as_bytes().to_vec();
                    (Some(bytes.clone()), Some(bytes))
                }
            }
            KeyCode::Enter => {
                self.char_input_len = 0;
                (Some(b"\r\n".to_vec()), Some(b"\r\n".to_vec()))
            }
            KeyCode::Backspace => {
                if self.char_input_len > 0 {
                    self.char_input_len -= 1;
                    // Send DEL (0x7F), not BS (0x08): xterm-family terminals
                    // (incl. macOS Terminal, iTerm2, gnome-terminal) emit DEL
                    // for the Backspace/Delete key, and modern telnet hosts
                    // and BBSes treat it as "erase last char". BS is the
                    // literal Ctrl+H byte and many BBSes render it as `^H`.
                    (Some(vec![0x7F]), Some(b"\x08 \x08".to_vec()))
                } else {
                    (None, None) // nothing to erase, don't send
                }
            }
            KeyCode::Esc => (None, None),
            KeyCode::Up => (Some(b"\x1b[A".to_vec()), None),
            KeyCode::Down => (Some(b"\x1b[B".to_vec()), None),
            KeyCode::Right => (Some(b"\x1b[C".to_vec()), None),
            KeyCode::Left => (Some(b"\x1b[D".to_vec()), None),
            KeyCode::Home => (Some(b"\x1b[H".to_vec()), None),
            KeyCode::End => (Some(b"\x1b[F".to_vec()), None),
            KeyCode::PageUp => (Some(b"\x1b[5~".to_vec()), None),
            KeyCode::PageDown => (Some(b"\x1b[6~".to_vec()), None),
            KeyCode::Delete => (Some(b"\x1b[3~".to_vec()), None),
            KeyCode::Insert => (Some(b"\x1b[2~".to_vec()), None),
            KeyCode::F(n) => (Some(f_key_escape(n)), None),
            _ => (None, None),
        };

        if let Some(echo_data) = echo
            && self.needs_local_echo()
        {
            self.emulator.process(&echo_data);
        }

        if let Some(data) = send {
            let _ = tx.send(ConnectionCommand::SendRaw(data)).await;
        }
        Ok(())
    }

    /// Pick the initial input mode for a new connection.
    fn initial_input_mode_for(
        entry: &AddressBookEntry,
        settings: &config::settings::Settings,
    ) -> InputMode {
        if let Some(mode) = entry.default_input_mode {
            return mode;
        }
        match entry.protocol {
            // Interactive shells / full-screen apps expect raw keys.
            Protocol::Ssh => InputMode::Character,
            Protocol::Telnet => settings.default_input_mode,
        }
    }

    /// If telnet negotiated server ECHO and the user hasn't Tab-toggled,
    /// switch to character mode (and update NAWS for the new chrome).
    async fn maybe_auto_switch_input_mode(&mut self) {
        if self.input_mode_user_set || self.input_mode == InputMode::Character {
            return;
        }
        // Honour an explicit per-entry line-mode override.
        if let Some(idx) = self.connected_entry
            && let Some(entry) = self.entries.get(idx)
            && entry.default_input_mode == Some(InputMode::LineBuffered)
        {
            return;
        }
        let Some(flags) = &self.telnet_flags else {
            return;
        };
        if !flags.server_echo.load(Ordering::Relaxed) {
            return;
        }
        self.input_mode = InputMode::Character;
        self.apply_viewport_to_connection().await;
    }

    async fn connect(&mut self) -> Result<()> {
        let Some(entry) = self.entries.get(self.selected) else {
            return Ok(());
        };
        let name = entry.name.clone();
        let host = entry.host.clone();
        let port = entry.port;
        let protocol = entry.protocol;
        let username = entry.username.clone();
        let entry_terminal_type = entry.terminal_type.clone();
        let initial_mode = Self::initial_input_mode_for(entry, &self.settings);

        // Cancel any existing connection
        self.cancel_connection().await;

        self.connection_id += 1;
        self.state = AppState::Connecting;
        self.input.clear();
        self.input_mode = initial_mode;
        self.input_mode_user_set = false;

        let (cols, rows) = self.terminal_viewport();
        let scrollback = self.settings.scrollback_lines;
        self.emulator = TerminalEmulator::new(rows, cols, scrollback);

        let id = self.connection_id;
        let event_tx = self.event_tx.clone();
        let cancel = CancellationToken::new();
        self.connection_cancel = Some(cancel.clone());

        let terminal_type =
            entry_terminal_type.unwrap_or_else(|| self.settings.terminal_type.clone());
        let handle = match protocol {
            Protocol::Telnet => {
                self.status_message = format!("Connecting to {}...", name);
                tokio::spawn(async move {
                    network::connect_raw_tcp(
                        host,
                        port,
                        cols,
                        rows,
                        id,
                        event_tx,
                        terminal_type,
                        cancel,
                    )
                    .await;
                })
            }
            Protocol::Ssh => {
                let resolved = network::ssh::resolve_ssh_username(username.as_deref());
                self.status_message = format!("Connecting to {} as {}...", name, resolved);
                tokio::spawn(async move {
                    network::ssh::connect_ssh(
                        host,
                        port,
                        username,
                        cols,
                        rows,
                        id,
                        event_tx,
                        terminal_type,
                        cancel,
                    )
                    .await;
                })
            }
        };
        self.connection_handle = Some(handle);
        Ok(())
    }

    async fn disconnect(&mut self) {
        self.cancel_connection().await;
    }

    /// Drop network tasks without awaiting channel send (used from sync
    /// delete path and as the core of `cancel_connection`).
    fn teardown_connection_sync(&mut self) {
        if let Some(token) = self.connection_cancel.take() {
            token.cancel();
        }
        // Closing the cmd channel also unblocks the writer select.
        self.connection_tx.take();
        if let Some(handle) = self.connection_handle.take() {
            handle.abort();
        }
    }

    async fn cancel_connection(&mut self) {
        // Prefer a graceful Disconnect so the peer sees EOF when possible.
        if let Some(tx) = self.connection_tx.take() {
            let _ = tx.send(ConnectionCommand::Disconnect).await;
        }
        if let Some(token) = self.connection_cancel.take() {
            token.cancel();
        }
        // Abort the outer task as a backstop for a stuck handshake.
        if let Some(handle) = self.connection_handle.take() {
            handle.abort();
        }
    }
}

fn f_key_escape(n: u8) -> Vec<u8> {
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => vec![],
    }
}

#[cfg(test)]
mod popup_tests {
    use super::*;

    fn entry() -> AddressBookEntry {
        AddressBookEntry {
            name: "x".into(),
            host: "h".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
            default_input_mode: None,
        }
    }

    #[test]
    fn add_starts_focused_on_name_with_default_telnet_port() {
        let f = FormPopup::new_add();
        assert_eq!(f.focused, PopupField::Name);
        assert_eq!(f.port_str, "23");
        assert_eq!(f.protocol, Protocol::Telnet);
    }

    #[test]
    fn next_field_cycles_through_all_fields() {
        let mut f = FormPopup::new_add();
        let order = [
            PopupField::Name,
            PopupField::Host,
            PopupField::Port,
            PopupField::Protocol,
            PopupField::Username,
            PopupField::TerminalType,
            PopupField::InputMode,
            PopupField::Name,
        ];
        for window in order.windows(2) {
            assert_eq!(f.focused, window[0]);
            f.next_field();
            assert_eq!(f.focused, window[1]);
        }
    }

    #[test]
    fn prev_field_is_inverse_of_next() {
        let mut f = FormPopup::new_add();
        for _ in 0..7 {
            f.next_field();
        }
        assert_eq!(f.focused, PopupField::Name);
        for _ in 0..7 {
            f.prev_field();
        }
        assert_eq!(f.focused, PopupField::Name);
    }

    #[test]
    fn form_popup_starts_terminal_type_on_default_sentinel() {
        let f = FormPopup::new_add();
        assert_eq!(f.terminal_type_label(), FORM_TERMINAL_TYPE_DEFAULT);
        assert!(f.terminal_type_override().is_none());
    }

    #[test]
    fn form_popup_cycles_terminal_type_through_default_then_standard_list() {
        let mut f = FormPopup::new_add();
        f.cycle_terminal_type();
        assert_eq!(f.terminal_type_label(), "xterm-256color");
        assert_eq!(
            f.terminal_type_override(),
            Some("xterm-256color".to_string())
        );
        // Cycle back around to the (default) sentinel
        let total = STANDARD_TERMINAL_TYPES.len();
        for _ in 0..total {
            f.cycle_terminal_type();
        }
        assert_eq!(f.terminal_type_label(), FORM_TERMINAL_TYPE_DEFAULT);
        assert!(f.terminal_type_override().is_none());
    }

    #[test]
    fn form_popup_edit_preserves_custom_terminal_type_at_head_of_cycle() {
        let entry = AddressBookEntry {
            name: "Custom".into(),
            host: "h".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: Some("rxvt-256color".into()),
            default_input_mode: None,
        };
        let f = FormPopup::new_edit(&entry);
        assert_eq!(f.terminal_type_label(), "rxvt-256color");
        assert_eq!(
            f.terminal_type_override(),
            Some("rxvt-256color".to_string())
        );
        // Custom value sits between the (default) sentinel and the standard list
        assert_eq!(f.terminal_type_options[0], FORM_TERMINAL_TYPE_DEFAULT);
        assert_eq!(f.terminal_type_options[1], "rxvt-256color");
    }

    #[test]
    fn form_popup_to_entry_carries_terminal_type_override() {
        let mut f = FormPopup::new_add();
        f.name = "Test".into();
        f.host = "example.com".into();
        f.cycle_terminal_type(); // (default) -> xterm-256color
        f.cycle_terminal_type(); // -> xterm
        f.cycle_terminal_type(); // -> ansi
        let entry = f.to_entry().expect("expected valid entry");
        assert_eq!(entry.terminal_type, Some("ansi".to_string()));
    }

    #[test]
    fn form_popup_to_entry_records_default_sentinel_as_none() {
        let mut f = FormPopup::new_add();
        f.name = "Test".into();
        f.host = "example.com".into();
        // Leave terminal_type on the (default) sentinel
        let entry = f.to_entry().expect("expected valid entry");
        assert!(entry.terminal_type.is_none());
    }

    #[test]
    fn type_char_appends_to_focused_text_field() {
        let mut f = FormPopup::new_add();
        f.type_char('a');
        f.type_char('b');
        assert_eq!(f.name, "ab");
        f.next_field();
        f.type_char('h');
        assert_eq!(f.host, "h");
    }

    #[test]
    fn type_char_into_protocol_field_is_noop() {
        // Bug being fixed: previously typing on Protocol field wrote to `name`
        // because active_field_mut() fell through to &mut self.name.
        let mut f = FormPopup::new_add();
        f.next_field();
        f.next_field();
        f.next_field(); // Name → Host → Port → Protocol
        assert_eq!(f.focused, PopupField::Protocol);
        let name_before = f.name.clone();
        f.type_char('z');
        assert_eq!(
            f.name, name_before,
            "typing on Protocol must not mutate Name"
        );
    }

    #[test]
    fn toggle_protocol_swaps_default_port() {
        let mut f = FormPopup::new_add();
        assert_eq!(f.protocol, Protocol::Telnet);
        assert_eq!(f.port_str, "23");
        f.toggle_protocol();
        assert_eq!(f.protocol, Protocol::Ssh);
        assert_eq!(f.port_str, "22");
        f.toggle_protocol();
        assert_eq!(f.protocol, Protocol::Telnet);
        assert_eq!(f.port_str, "23");
    }

    #[test]
    fn toggle_protocol_swaps_modernbbs_ports() {
        let mut f = FormPopup::new_add();
        f.port_str = "2323".into();
        f.toggle_protocol();
        assert_eq!(f.protocol, Protocol::Ssh);
        assert_eq!(f.port_str, "2222");
        f.toggle_protocol();
        assert_eq!(f.protocol, Protocol::Telnet);
        assert_eq!(f.port_str, "2323");
    }

    #[test]
    fn toggle_protocol_preserves_custom_port() {
        let mut f = FormPopup::new_add();
        f.port_str = "9999".into();
        f.toggle_protocol();
        assert_eq!(f.port_str, "9999");
    }

    #[test]
    fn form_popup_cycles_input_mode_and_to_entry() {
        let mut f = FormPopup::new_add();
        f.name = "n".into();
        f.host = "h".into();
        assert_eq!(f.input_mode_label(), FORM_INPUT_MODE_DEFAULT);
        assert!(f.input_mode_override().is_none());
        f.cycle_input_mode();
        assert_eq!(f.input_mode_override(), Some(InputMode::LineBuffered));
        f.cycle_input_mode();
        assert_eq!(f.input_mode_override(), Some(InputMode::Character));
        let entry = f.to_entry().unwrap();
        assert_eq!(entry.default_input_mode, Some(InputMode::Character));
    }

    #[test]
    fn initial_input_mode_ssh_defaults_to_character() {
        let entry = AddressBookEntry {
            name: "s".into(),
            host: "h".into(),
            port: 22,
            protocol: Protocol::Ssh,
            username: None,
            terminal_type: None,
            default_input_mode: None,
        };
        let settings = config::settings::Settings::default();
        assert_eq!(
            App::initial_input_mode_for(&entry, &settings),
            InputMode::Character
        );
    }

    #[test]
    fn initial_input_mode_honours_entry_override() {
        let entry = AddressBookEntry {
            name: "s".into(),
            host: "h".into(),
            port: 22,
            protocol: Protocol::Ssh,
            username: None,
            terminal_type: None,
            default_input_mode: Some(InputMode::LineBuffered),
        };
        let settings = config::settings::Settings::default();
        assert_eq!(
            App::initial_input_mode_for(&entry, &settings),
            InputMode::LineBuffered
        );
    }

    #[test]
    fn terminal_viewport_matches_input_mode_chrome() {
        let (tx, _rx) = mpsc::channel(1);
        let mut app = App::new(tx);
        app.width = 80;
        app.height = 24;
        app.input_mode = InputMode::LineBuffered;
        assert_eq!(app.terminal_viewport(), (80, 20)); // 24 - 4
        app.input_mode = InputMode::Character;
        assert_eq!(app.terminal_viewport(), (80, 23)); // 24 - 1
    }

    #[test]
    fn to_entry_rejects_empty_name_or_host_or_bad_port() {
        let mut f = FormPopup::new_add();
        assert!(f.to_entry().is_none(), "empty name/host should fail");
        f.name = "n".into();
        assert!(f.to_entry().is_none(), "empty host should fail");
        f.host = "h".into();
        assert!(f.to_entry().is_some(), "valid form should pass");
        f.port_str = "not-a-number".into();
        assert!(f.to_entry().is_none(), "bad port should fail");
    }

    #[test]
    fn new_edit_prefills_from_entry() {
        let f = FormPopup::new_edit(&entry());
        assert_eq!(f.name, "x");
        assert_eq!(f.host, "h");
        assert_eq!(f.port_str, "23");
    }

    #[test]
    fn settings_popup_round_trips_focus_with_tab() {
        let s = config::settings::Settings::default();
        let mut p = EditSettingsPopup::from_settings(&s);
        assert_eq!(p.focused, SettingsField::Scrollback);
        p.next_field();
        assert_eq!(p.focused, SettingsField::Mode);
        p.next_field();
        assert_eq!(p.focused, SettingsField::TerminalType);
        p.next_field();
        assert_eq!(p.focused, SettingsField::Scrollback);
        p.prev_field();
        assert_eq!(p.focused, SettingsField::TerminalType);
    }

    #[test]
    fn settings_popup_toggle_mode_swaps_input_mode() {
        let s = config::settings::Settings::default();
        let mut p = EditSettingsPopup::from_settings(&s);
        assert_eq!(p.mode, InputMode::LineBuffered);
        p.toggle_mode();
        assert_eq!(p.mode, InputMode::Character);
        p.toggle_mode();
        assert_eq!(p.mode, InputMode::LineBuffered);
    }

    #[test]
    fn settings_popup_rejects_non_numeric_scrollback() {
        let s = config::settings::Settings::default();
        let mut p = EditSettingsPopup::from_settings(&s);
        p.scrollback_input = "abc".into();
        assert!(p.validate().is_none());
        assert!(p.error.is_some());
    }

    #[test]
    fn settings_popup_rejects_scrollback_above_cap() {
        let s = config::settings::Settings::default();
        let mut p = EditSettingsPopup::from_settings(&s);
        p.scrollback_input = "100001".into();
        assert!(p.validate().is_none());
        assert!(p.error.is_some());
    }

    #[test]
    fn settings_popup_cycles_terminal_type_through_standard_list() {
        let s = config::settings::Settings::default();
        let mut p = EditSettingsPopup::from_settings(&s);
        // Default value is the first standard option
        assert_eq!(p.terminal_type_value(), "xterm-256color");
        p.cycle_terminal_type();
        assert_eq!(p.terminal_type_value(), "xterm");
        p.cycle_terminal_type();
        assert_eq!(p.terminal_type_value(), "ansi");
        // Cycle all the way back around
        for _ in 0..STANDARD_TERMINAL_TYPES.len() {
            p.cycle_terminal_type();
        }
        assert_eq!(p.terminal_type_value(), "ansi");
    }

    #[test]
    fn settings_popup_preserves_custom_terminal_type_at_head_of_cycle() {
        let s = config::settings::Settings {
            scrollback_lines: 1000,
            default_input_mode: InputMode::LineBuffered,
            terminal_type: "rxvt-256color".into(),
        };
        let p = EditSettingsPopup::from_settings(&s);
        assert_eq!(p.terminal_type_value(), "rxvt-256color");
        // Custom value sits in front of the standard list
        assert_eq!(
            p.terminal_type_options.len(),
            STANDARD_TERMINAL_TYPES.len() + 1
        );
        assert_eq!(p.terminal_type_options[0], "rxvt-256color");
    }

    #[test]
    fn settings_popup_validate_returns_settings_on_valid_input() {
        let s = config::settings::Settings::default();
        let mut p = EditSettingsPopup::from_settings(&s);
        p.scrollback_input = "4242".into();
        // Cycle terminal type to "ansi" (index 2 in the standard list)
        p.cycle_terminal_type();
        p.cycle_terminal_type();
        p.toggle_mode();
        let out = p.validate().expect("expected Ok validation");
        assert_eq!(out.scrollback_lines, 4242);
        assert_eq!(out.default_input_mode, InputMode::Character);
        assert_eq!(out.terminal_type, "ansi");
        assert!(p.error.is_none());
    }
}

/// Reliability tests for the App state machine. Drive `handle_app_event` and
/// key handling through channels — no TTY required.
#[cfg(test)]
mod app_state_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    fn sample_entry() -> AddressBookEntry {
        AddressBookEntry {
            name: "test".into(),
            host: "localhost".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
            default_input_mode: None,
        }
    }

    fn test_app(entries: Vec<AddressBookEntry>) -> (App, mpsc::Receiver<AppEvent>) {
        let (event_tx, event_rx) = mpsc::channel(16);
        let mut settings = config::settings::Settings::default();
        // Line mode by default so WILL-ECHO auto-switch tests are meaningful.
        settings.default_input_mode = InputMode::LineBuffered;
        let app = App::new_for_test(event_tx, entries, settings);
        (app, event_rx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Put the app in Connected with a live cmd channel and optional telnet flags.
    /// Uses the current `app.selected` as `connected_entry`.
    async fn simulate_connected(
        app: &mut App,
        id: u64,
        flags: Option<Arc<TelnetFlags>>,
    ) -> mpsc::Receiver<ConnectionCommand> {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        app.connection_id = id;
        app.input_mode = InputMode::LineBuffered;
        app.input_mode_user_set = false;
        app.handle_app_event(AppEvent::Connected {
            id,
            cmd_tx,
            telnet_flags: flags,
        })
        .await
        .unwrap();
        cmd_rx
    }

    fn drain_raw(rx: &mut mpsc::Receiver<ConnectionCommand>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let ConnectionCommand::SendRaw(b) = cmd {
                out.push(b);
            }
        }
        out
    }

    fn drain_resizes(rx: &mut mpsc::Receiver<ConnectionCommand>) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let ConnectionCommand::Resize(c, r) = cmd {
                out.push((c, r));
            }
        }
        out
    }

    #[tokio::test]
    async fn connected_with_matching_id_enters_connected_state() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;
        assert_eq!(app.state, AppState::Connected);
        assert!(app.connection_tx.is_some());
        assert_eq!(app.connected_entry, Some(0));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_connected_event_is_ignored() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.connection_id = 5;
        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        app.handle_app_event(AppEvent::Connected {
            id: 4, // stale
            cmd_tx,
            telnet_flags: None,
        })
        .await
        .unwrap();
        assert_eq!(app.state, AppState::AddressBook);
        assert!(app.connection_tx.is_none());
    }

    #[tokio::test]
    async fn stale_network_data_does_not_update_emulator() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;
        let before = app.emulator.cursor_position();
        app.handle_app_event(AppEvent::NetworkData {
            id: 99,
            data: b"hello".to_vec(),
        })
        .await
        .unwrap();
        assert_eq!(app.emulator.cursor_position(), before);
        assert!(drain_raw(&mut cmd_rx).is_empty());
    }

    #[tokio::test]
    async fn matching_network_data_updates_emulator() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.handle_app_event(AppEvent::NetworkData {
            id: 1,
            data: b"hello".to_vec(),
        })
        .await
        .unwrap();
        assert_eq!(app.emulator.cursor_position(), (0, 5));
    }

    #[tokio::test]
    async fn disconnected_with_matching_id_clears_session() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.handle_app_event(AppEvent::Disconnected {
            id: 1,
            reason: Some("bye".into()),
        })
        .await
        .unwrap();
        assert_eq!(app.state, AppState::AddressBook);
        assert!(app.connection_tx.is_none());
        assert!(app.telnet_flags.is_none());
        assert_eq!(app.connected_entry, None);
        assert!(app.status_message.contains("bye"));
    }

    #[tokio::test]
    async fn stale_disconnected_does_not_clear_live_session() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 2, None).await;
        app.handle_app_event(AppEvent::Disconnected {
            id: 1, // previous connection
            reason: Some("old".into()),
        })
        .await
        .unwrap();
        assert_eq!(app.state, AppState::Connected);
        assert!(app.connection_tx.is_some());
        assert_eq!(app.connected_entry, Some(0));
    }

    #[tokio::test]
    async fn esc_during_connecting_bumps_id_so_queued_connected_is_stale() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.connection_id = 7;
        app.state = AppState::Connecting;

        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Esc)))
            .await
            .unwrap();

        assert_eq!(app.state, AppState::AddressBook);
        assert_eq!(app.connection_id, 8);

        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        app.handle_app_event(AppEvent::Connected {
            id: 7, // the aborted attempt
            cmd_tx,
            telnet_flags: None,
        })
        .await
        .unwrap();
        assert_eq!(
            app.state,
            AppState::AddressBook,
            "Connected for cancelled attempt must not revive the session"
        );
        assert!(app.connection_tx.is_none());
    }

    #[tokio::test]
    async fn esc_while_connected_suspends_without_dropping_connection() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;

        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Esc)))
            .await
            .unwrap();

        assert_eq!(app.state, AppState::AddressBook);
        assert!(
            app.connection_tx.is_some(),
            "suspend must keep the network channel alive"
        );
        assert_eq!(app.connected_entry, Some(0));
        assert!(app.status_message.to_lowercase().contains("suspend"));
    }

    #[tokio::test]
    async fn cpr_samples_cursor_at_each_query_not_final_position() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;

        // Home, CPR, print 'X', CPR — second CPR must see col 1 (1-based col 2).
        let data = b"\x1b[1;1H\x1b[6nX\x1b[6n".to_vec();
        app.handle_app_event(AppEvent::NetworkData { id: 1, data })
            .await
            .unwrap();

        let replies = drain_raw(&mut cmd_rx);
        assert_eq!(
            replies.len(),
            2,
            "expected two CPR replies, got {replies:?}"
        );
        assert_eq!(replies[0], b"\x1b[1;1R", "first CPR at home");
        assert_eq!(
            replies[1], b"\x1b[1;2R",
            "second CPR after 'X' must report advanced column"
        );
    }

    #[tokio::test]
    async fn will_echo_auto_switches_to_character_mode_and_sends_resize() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let flags = Arc::new(TelnetFlags::new());
        let mut cmd_rx = simulate_connected(&mut app, 1, Some(flags.clone())).await;
        assert_eq!(app.input_mode, InputMode::LineBuffered);

        flags.server_echo.store(true, Ordering::Relaxed);
        app.handle_app_event(AppEvent::NetworkData {
            id: 1,
            data: b"".to_vec(),
        })
        .await
        .unwrap();

        assert_eq!(app.input_mode, InputMode::Character);
        let resizes = drain_resizes(&mut cmd_rx);
        assert!(
            !resizes.is_empty(),
            "auto-switch to char mode must notify peer of new viewport"
        );
        let (cols, rows) = app.terminal_viewport();
        assert_eq!(*resizes.last().unwrap(), (cols, rows));
    }

    #[tokio::test]
    async fn user_tab_suppresses_will_echo_auto_switch() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let flags = Arc::new(TelnetFlags::new());
        let mut cmd_rx = simulate_connected(&mut app, 1, Some(flags.clone())).await;

        // Tab → Character, Tab → Line (user-owned choice).
        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Tab)))
            .await
            .unwrap();
        assert_eq!(app.input_mode, InputMode::Character);
        assert!(app.input_mode_user_set);
        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Tab)))
            .await
            .unwrap();
        assert_eq!(app.input_mode, InputMode::LineBuffered);
        let _ = drain_resizes(&mut cmd_rx);

        flags.server_echo.store(true, Ordering::Relaxed);
        app.handle_app_event(AppEvent::NetworkData {
            id: 1,
            data: b"x".to_vec(),
        })
        .await
        .unwrap();

        assert_eq!(
            app.input_mode,
            InputMode::LineBuffered,
            "user Tab choice must not be overridden by WILL ECHO"
        );
    }

    #[tokio::test]
    async fn entry_line_mode_override_blocks_will_echo_auto_switch() {
        let mut entry = sample_entry();
        entry.default_input_mode = Some(InputMode::LineBuffered);
        let (mut app, _erx) = test_app(vec![entry]);
        let flags = Arc::new(TelnetFlags::new());
        let _cmd_rx = simulate_connected(&mut app, 1, Some(flags.clone())).await;
        // connected_entry set by Connected handler
        assert_eq!(app.connected_entry, Some(0));

        flags.server_echo.store(true, Ordering::Relaxed);
        app.handle_app_event(AppEvent::NetworkData {
            id: 1,
            data: b"".to_vec(),
        })
        .await
        .unwrap();
        assert_eq!(app.input_mode, InputMode::LineBuffered);
    }

    #[tokio::test]
    async fn tab_toggle_sends_viewport_resize_matching_chrome() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.width = 80;
        app.height = 24;
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;
        assert_eq!(app.input_mode, InputMode::LineBuffered);
        assert_eq!(app.terminal_viewport(), (80, 20));

        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Tab)))
            .await
            .unwrap();
        assert_eq!(app.input_mode, InputMode::Character);
        assert_eq!(app.terminal_viewport(), (80, 23));

        let resizes = drain_resizes(&mut cmd_rx);
        assert_eq!(resizes.last().copied(), Some((80, 23)));
    }

    #[tokio::test]
    async fn terminal_resize_event_sends_viewport_not_raw_height() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.width = 80;
        app.height = 24;
        app.input_mode = InputMode::LineBuffered;
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;

        app.handle_crossterm_event(CrosstermEvent::Resize(100, 40))
            .await
            .unwrap();
        // Line mode chrome = 4 → rows 36, not raw 40.
        let resizes = drain_resizes(&mut cmd_rx);
        assert_eq!(resizes.last().copied(), Some((100, 36)));
        assert_eq!(app.width, 100);
        assert_eq!(app.height, 40);
    }

    #[tokio::test]
    async fn password_needed_opens_popup_only_for_matching_id() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.connection_id = 3;
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        app.handle_app_event(AppEvent::PasswordNeeded {
            id: 1,
            reply: reply_tx,
        })
        .await
        .unwrap();
        assert!(app.popup.is_none());

        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        app.handle_app_event(AppEvent::PasswordNeeded {
            id: 3,
            reply: reply_tx,
        })
        .await
        .unwrap();
        assert!(matches!(app.popup, Some(Popup::Password(_))));
    }

    #[tokio::test]
    async fn host_key_mismatch_bumps_connection_id() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.connection_id = 10;
        app.state = AppState::Connecting;
        app.handle_app_event(AppEvent::HostKeyMismatch {
            id: 10,
            host: "h".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            stored_fingerprint: "a".into(),
            received_fingerprint: "b".into(),
            file_path: std::path::PathBuf::from("/tmp/kh"),
        })
        .await
        .unwrap();
        assert_eq!(app.connection_id, 11);
        assert!(app.status_message.contains("HOST KEY MISMATCH"));
        // Subsequent Disconnected for id 10 must not clobber the banner.
        app.handle_app_event(AppEvent::Disconnected {
            id: 10,
            reason: Some("abort".into()),
        })
        .await
        .unwrap();
        assert!(app.status_message.contains("HOST KEY MISMATCH"));
    }

    fn two_entries() -> Vec<AddressBookEntry> {
        vec![
            AddressBookEntry {
                name: "first".into(),
                host: "a.example".into(),
                port: 23,
                protocol: Protocol::Telnet,
                username: None,
                terminal_type: None,
                default_input_mode: None,
            },
            AddressBookEntry {
                name: "second".into(),
                host: "b.example".into(),
                port: 23,
                protocol: Protocol::Telnet,
                username: None,
                terminal_type: None,
                default_input_mode: None,
            },
        ]
    }

    #[tokio::test]
    async fn delete_lower_index_decrements_connected_entry() {
        let (mut app, _erx) = test_app(two_entries());
        // Connect to the second entry (index 1).
        app.selected = 1;
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        assert_eq!(app.connected_entry, Some(1));

        // Suspend, select first entry, confirm delete.
        app.state = AppState::AddressBook;
        app.selected = 0;
        app.popup = Some(Popup::DeleteConfirm);
        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Char('y'))))
            .await
            .unwrap();

        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].name, "second");
        assert_eq!(
            app.connected_entry,
            Some(0),
            "connected index must slide down with the live entry"
        );
        assert!(
            app.connection_tx.is_some(),
            "session for the remaining entry must stay live"
        );
    }

    #[tokio::test]
    async fn delete_connected_entry_tears_down_session() {
        let (mut app, _erx) = test_app(two_entries());
        app.selected = 0;
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        let id_before = app.connection_id;

        app.state = AppState::AddressBook;
        app.selected = 0;
        app.popup = Some(Popup::DeleteConfirm);
        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Char('y'))))
            .await
            .unwrap();

        assert_eq!(app.connected_entry, None);
        assert!(app.connection_tx.is_none());
        assert_eq!(app.connection_id, id_before.wrapping_add(1));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].name, "second");
    }

    #[tokio::test]
    async fn cancel_connection_fires_cancellation_token() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let token = CancellationToken::new();
        let child = token.child_token();
        app.connection_cancel = Some(token);
        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        app.connection_tx = Some(cmd_tx);
        app.connection_id = 3;

        app.cancel_connection().await;

        assert!(child.is_cancelled());
        assert!(app.connection_cancel.is_none());
        assert!(app.connection_tx.is_none());
    }

    #[tokio::test]
    async fn delete_higher_index_leaves_connected_entry_unchanged() {
        let (mut app, _erx) = test_app(two_entries());
        app.selected = 0;
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        assert_eq!(app.connected_entry, Some(0));

        app.state = AppState::AddressBook;
        app.selected = 1; // delete the *other* entry
        app.popup = Some(Popup::DeleteConfirm);
        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Char('y'))))
            .await
            .unwrap();

        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].name, "first");
        assert_eq!(app.connected_entry, Some(0));
        assert!(app.connection_tx.is_some());
    }

    #[tokio::test]
    async fn enter_on_connected_entry_resumes_suspended_session() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.state = AppState::AddressBook; // suspended
        app.selected = 0;
        assert_eq!(app.connected_entry, Some(0));

        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Enter)))
            .await
            .unwrap();

        assert_eq!(app.state, AppState::Connected);
        assert!(app.connection_tx.is_some());
        assert!(app.status_message.contains("Connected"));
    }

    #[test]
    fn initial_input_mode_telnet_uses_settings_when_no_override() {
        let entry = AddressBookEntry {
            name: "t".into(),
            host: "h".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
            default_input_mode: None,
        };
        let mut settings = config::settings::Settings::default();
        settings.default_input_mode = InputMode::Character;
        assert_eq!(
            App::initial_input_mode_for(&entry, &settings),
            InputMode::Character
        );
        settings.default_input_mode = InputMode::LineBuffered;
        assert_eq!(
            App::initial_input_mode_for(&entry, &settings),
            InputMode::LineBuffered
        );
    }

    #[test]
    fn form_edit_prefills_input_mode_override() {
        let entry = AddressBookEntry {
            name: "x".into(),
            host: "h".into(),
            port: 23,
            protocol: Protocol::Telnet,
            username: None,
            terminal_type: None,
            default_input_mode: Some(InputMode::Character),
        };
        let f = FormPopup::new_edit(&entry);
        assert_eq!(f.input_mode_override(), Some(InputMode::Character));
        assert_eq!(f.input_mode_label(), "character");
    }

    #[tokio::test]
    async fn dsr_and_da_queries_get_replies() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.handle_app_event(AppEvent::NetworkData {
            id: 1,
            data: b"\x1b[5n\x1b[c".to_vec(),
        })
        .await
        .unwrap();
        let replies = drain_raw(&mut cmd_rx);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], b"\x1b[0n");
        assert_eq!(replies[1], b"\x1b[?1;2c");
    }

    #[tokio::test]
    async fn character_mode_connect_viewport_is_taller_than_line_mode() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.width = 80;
        app.height = 30;
        app.input_mode = InputMode::LineBuffered;
        let line = app.terminal_viewport();
        app.input_mode = InputMode::Character;
        let ch = app.terminal_viewport();
        assert_eq!(line, (80, 26)); // 30 - 4
        assert_eq!(ch, (80, 29)); // 30 - 1
        assert!(ch.1 > line.1);
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[tokio::test]
    async fn char_mode_forwards_ctrl_c_and_ctrl_d_to_remote() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.input_mode = InputMode::Character;

        app.handle_crossterm_event(CrosstermEvent::Key(ctrl('c')))
            .await
            .unwrap();
        app.handle_crossterm_event(CrosstermEvent::Key(ctrl('d')))
            .await
            .unwrap();

        let raw = drain_raw(&mut cmd_rx);
        assert_eq!(raw, vec![vec![0x03], vec![0x04]]);
        assert!(!app.should_quit());
        assert_eq!(app.state, AppState::Connected);
        assert!(app.connection_tx.is_some());
    }

    #[tokio::test]
    async fn line_mode_ctrl_d_disconnects() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        assert_eq!(app.input_mode, InputMode::LineBuffered);

        app.handle_crossterm_event(CrosstermEvent::Key(ctrl('d')))
            .await
            .unwrap();

        assert_eq!(app.state, AppState::AddressBook);
        assert!(app.connection_tx.is_none());
        assert_eq!(app.connected_entry, None);
    }

    #[tokio::test]
    async fn line_mode_ctrl_c_quits() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.input_mode = InputMode::LineBuffered;

        app.handle_crossterm_event(CrosstermEvent::Key(ctrl('c')))
            .await
            .unwrap();

        assert!(app.should_quit());
    }

    #[tokio::test]
    async fn chord_q_quits_from_character_mode() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.input_mode = InputMode::Character;

        // Ctrl+] then q
        app.handle_crossterm_event(CrosstermEvent::Key(ctrl(']')))
            .await
            .unwrap();
        assert_eq!(app.chord, ChordMode::Awaiting);
        app.handle_crossterm_event(CrosstermEvent::Key(key(KeyCode::Char('q'))))
            .await
            .unwrap();

        assert!(app.should_quit());
        assert!(app.connection_tx.is_none());
    }

    #[tokio::test]
    async fn paste_into_line_mode_appends_to_input() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let _cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.input_mode = InputMode::LineBuffered;
        app.input = "pre".into();

        app.handle_crossterm_event(CrosstermEvent::Paste("hello\nworld".into()))
            .await
            .unwrap();

        assert_eq!(app.input, "prehelloworld");
    }

    #[tokio::test]
    async fn paste_into_char_mode_sends_normalized_raw() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        let mut cmd_rx = simulate_connected(&mut app, 1, None).await;
        app.input_mode = InputMode::Character;

        app.handle_crossterm_event(CrosstermEvent::Paste("ab\r\ncd\nef".into()))
            .await
            .unwrap();

        let raw = drain_raw(&mut cmd_rx);
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0], b"ab\r\ncd\r\nef".to_vec());
    }

    #[tokio::test]
    async fn paste_into_password_popup() {
        let (mut app, _erx) = test_app(vec![sample_entry()]);
        app.connection_id = 1;
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        app.handle_app_event(AppEvent::PasswordNeeded {
            id: 1,
            reply: reply_tx,
        })
        .await
        .unwrap();

        app.handle_crossterm_event(CrosstermEvent::Paste("s3cret\n".into()))
            .await
            .unwrap();

        match &app.popup {
            Some(Popup::Password(pw)) => assert_eq!(pw, "s3cret"),
            _ => panic!("expected password popup"),
        }
    }
}
