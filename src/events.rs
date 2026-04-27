use std::sync::Arc;

use tokio::sync::mpsc;

use crate::network::telnet::TelnetFlags;

/// Commands sent from the app to the network connection.
#[derive(Debug)]
pub enum ConnectionCommand {
    SendText(String),
    SendRaw(Vec<u8>),
    Resize(u16, u16),
    Disconnect,
}

/// Events flowing into the main event loop.
/// Each event carries a `connection_id` so stale events from
/// abandoned connections can be ignored.
pub enum AppEvent {
    Connected {
        id: u64,
        cmd_tx: mpsc::Sender<ConnectionCommand>,
        telnet_flags: Option<Arc<TelnetFlags>>,
    },
    NetworkData {
        id: u64,
        data: Vec<u8>,
    },
    Disconnected {
        id: u64,
        reason: Option<String>,
    },
    PasswordNeeded {
        id: u64,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    HostKeyTrustNeeded {
        id: u64,
        host: String,
        port: u16,
        key_type: String,
        fingerprint: String,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    HostKeyMismatch {
        id: u64,
        host: String,
        port: u16,
        key_type: String,
        stored_fingerprint: String,
        received_fingerprint: String,
        file_path: std::path::PathBuf,
    },
}
