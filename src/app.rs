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
}

/// Sentinel shown at the head of the form popup's terminal-type cycle. When
/// the user leaves it on this option, the entry stores `terminal_type: None`,
/// which tells `App::connect` to fall back to whatever Settings says.
const FORM_TERMINAL_TYPE_DEFAULT: &str = "(default)";

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
}

impl FormPopup {
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
        }
    }

    pub fn new_edit(entry: &AddressBookEntry) -> Self {
        let options = form_terminal_type_options(entry.terminal_type.as_deref());
        let terminal_type_idx = match &entry.terminal_type {
            None => 0,
            Some(s) => options.iter().position(|o| o == s).unwrap_or(0),
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
        }
    }

    pub fn next_field(&mut self) {
        self.focused = match self.focused {
            PopupField::Name => PopupField::Host,
            PopupField::Host => PopupField::Port,
            PopupField::Port => PopupField::Protocol,
            PopupField::Protocol => PopupField::Username,
            PopupField::Username => PopupField::TerminalType,
            PopupField::TerminalType => PopupField::Name,
        };
    }

    pub fn prev_field(&mut self) {
        self.focused = match self.focused {
            PopupField::Name => PopupField::TerminalType,
            PopupField::Host => PopupField::Name,
            PopupField::Port => PopupField::Host,
            PopupField::Protocol => PopupField::Port,
            PopupField::Username => PopupField::Protocol,
            PopupField::TerminalType => PopupField::Username,
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
                }
                Protocol::Ssh
            }
            Protocol::Ssh => {
                if self.port_str == "22" {
                    self.port_str = "23".into();
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
        })
    }

    fn text_field_mut(&mut self) -> Option<&mut String> {
        match self.focused {
            PopupField::Name => Some(&mut self.name),
            PopupField::Host => Some(&mut self.host),
            PopupField::Port => Some(&mut self.port_str),
            PopupField::Username => Some(&mut self.username),
            PopupField::Protocol | PopupField::TerminalType => None,
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
    password_reply: Option<tokio::sync::oneshot::Sender<String>>,
    host_key_reply: Option<tokio::sync::oneshot::Sender<bool>>,
    pub capture: Option<config::capture::CaptureFile>,
    ansi_query_scanner: AnsiQueryScanner,
    chord: ChordMode,
    shown_chord_hint: bool,
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
            password_reply: None,
            host_key_reply: None,
            capture: None,
            ansi_query_scanner: AnsiQueryScanner::new(),
            chord: ChordMode::Normal,
            shown_chord_hint: false,
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
                // Send NAWS update to server if connected
                if let Some(tx) = &self.connection_tx {
                    let _ = tx.send(ConnectionCommand::Resize(w, h)).await;
                }
            }
            _ => {}
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
                let name = self.entries[self.selected].name.clone();
                self.entries.remove(self.selected);
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
                // Detect ANSI query sequences (CPR / DSR / DA) before
                // processing so we know whether to reply at all. The
                // emulator still sees the full byte stream — vt100 treats
                // these queries as no-ops to screen state.
                let queries = self.ansi_query_scanner.scan(&data);
                // Process data even when viewing address book (session suspended)
                self.emulator.process(&data);
                if !queries.is_empty()
                    && let Some(tx) = &self.connection_tx
                {
                    for q in queries {
                        let response = match q {
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
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
            KeyCode::Up | KeyCode::Char('k') if self.selected > 0 => {
                self.selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < self.entries.len() => {
                self.selected += 1;
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

        // Ctrl+D: explicit disconnect
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            self.disconnect().await;
            self.connected_entry = None;
            self.state = AppState::AddressBook;
            self.status_message = "Disconnected".into();
            return Ok(());
        }

        // Tab toggles input mode
        if key.code == KeyCode::Tab && key.modifiers.is_empty() {
            self.input_mode = match self.input_mode {
                InputMode::LineBuffered => InputMode::Character,
                InputMode::Character => InputMode::LineBuffered,
            };
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
                    let ctrl = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                    (Some(vec![ctrl]), None)
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

        // Cancel any existing connection
        self.cancel_connection().await;

        self.connection_id += 1;
        self.state = AppState::Connecting;
        self.status_message = format!("Connecting to {}...", name);
        self.input.clear();

        let term_height = self.height.saturating_sub(4);
        let term_width = self.width.max(1);
        let scrollback = self.settings.scrollback_lines;
        self.emulator = TerminalEmulator::new(term_height.max(1), term_width, scrollback);
        self.input_mode = self.settings.default_input_mode;

        let id = self.connection_id;
        let event_tx = self.event_tx.clone();

        let cols = term_width;
        let rows = term_height.max(1);
        let terminal_type =
            entry_terminal_type.unwrap_or_else(|| self.settings.terminal_type.clone());
        let handle = match protocol {
            Protocol::Telnet => tokio::spawn(async move {
                network::connect_raw_tcp(host, port, cols, rows, id, event_tx, terminal_type).await;
            }),
            Protocol::Ssh => tokio::spawn(async move {
                network::ssh::connect_ssh(
                    host,
                    port,
                    username,
                    cols,
                    rows,
                    id,
                    event_tx,
                    terminal_type,
                )
                .await;
            }),
        };
        self.connection_handle = Some(handle);
        Ok(())
    }

    async fn disconnect(&mut self) {
        self.cancel_connection().await;
    }

    async fn cancel_connection(&mut self) {
        // Send disconnect command if we have an active connection
        if let Some(tx) = self.connection_tx.take() {
            let _ = tx.send(ConnectionCommand::Disconnect).await;
        }
        // Abort the background task (cancels pending TCP connect too)
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
        for _ in 0..6 {
            f.next_field();
        }
        assert_eq!(f.focused, PopupField::Name);
        for _ in 0..6 {
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
    fn toggle_protocol_preserves_custom_port() {
        let mut f = FormPopup::new_add();
        f.port_str = "9999".into();
        f.toggle_protocol();
        assert_eq!(f.port_str, "9999");
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
