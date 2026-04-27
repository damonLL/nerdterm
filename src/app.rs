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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FormMode {
    Add,
    Edit,
}

pub enum Popup {
    Form(FormPopup),
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
        }
    }

    pub fn new_edit(entry: &AddressBookEntry) -> Self {
        Self {
            mode: FormMode::Edit,
            focused: PopupField::Name,
            name: entry.name.clone(),
            host: entry.host.clone(),
            port_str: entry.port.to_string(),
            protocol: entry.protocol,
            username: entry.username.clone().unwrap_or_default(),
        }
    }

    pub fn next_field(&mut self) {
        self.focused = match self.focused {
            PopupField::Name => PopupField::Host,
            PopupField::Host => PopupField::Port,
            PopupField::Port => PopupField::Protocol,
            PopupField::Protocol => PopupField::Username,
            PopupField::Username => PopupField::Name,
        };
    }

    pub fn prev_field(&mut self) {
        self.focused = match self.focused {
            PopupField::Name => PopupField::Username,
            PopupField::Host => PopupField::Name,
            PopupField::Port => PopupField::Host,
            PopupField::Protocol => PopupField::Port,
            PopupField::Username => PopupField::Protocol,
        };
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
        })
    }

    fn text_field_mut(&mut self) -> Option<&mut String> {
        match self.focused {
            PopupField::Name => Some(&mut self.name),
            PopupField::Host => Some(&mut self.host),
            PopupField::Port => Some(&mut self.port_str),
            PopupField::Username => Some(&mut self.username),
            PopupField::Protocol => None,
        }
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
            Some(Popup::Password(_)) => self.handle_key_password_popup(key),
            Some(Popup::DeleteConfirm) => self.handle_key_delete_popup(key),
            Some(Popup::Form(_)) => self.handle_key_form_popup(key),
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
                // Process data even when viewing address book (session suspended)
                self.emulator.process(&data);
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

        let id = self.connection_id;
        let event_tx = self.event_tx.clone();

        let cols = term_width;
        let rows = term_height.max(1);
        let terminal_type = self.settings.terminal_type.clone();
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
        for _ in 0..5 {
            f.next_field();
        }
        assert_eq!(f.focused, PopupField::Name);
        for _ in 0..5 {
            f.prev_field();
        }
        assert_eq!(f.focused, PopupField::Name);
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
}
