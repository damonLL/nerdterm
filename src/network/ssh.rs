use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::{AppEvent, ConnectionCommand};
use crate::network::CONNECT_TIMEOUT;

struct SshHandler {
    host: String,
    port: u16,
    connection_id: u64,
    event_tx: mpsc::Sender<AppEvent>,
}

impl client::Handler for SshHandler {
    type Error = anyhow::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let host = self.host.clone();
        let port = self.port;
        let id = self.connection_id;
        let event_tx = self.event_tx.clone();
        let key_type = server_public_key.algorithm().as_str().to_string();
        let fingerprint = server_public_key
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();

        async move {
            let loaded = crate::config::known_hosts::load();
            // Startup-surfaced warnings live elsewhere; we drop any here.
            let mut known = loaded.known_hosts;

            match known.verify(&host, port, &key_type, &fingerprint) {
                crate::config::known_hosts::Verdict::Trusted => Ok(true),
                crate::config::known_hosts::Verdict::Mismatch { stored } => {
                    let file_path = crate::config::known_hosts::KnownHosts::path()
                        .unwrap_or_else(|_| std::path::PathBuf::from("known_hosts.toml"));
                    let _ = event_tx
                        .send(AppEvent::HostKeyMismatch {
                            id,
                            host,
                            port,
                            key_type,
                            stored_fingerprint: stored,
                            received_fingerprint: fingerprint,
                            file_path,
                        })
                        .await;
                    Ok(false)
                }
                crate::config::known_hosts::Verdict::Unknown => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if event_tx
                        .send(AppEvent::HostKeyTrustNeeded {
                            id,
                            host: host.clone(),
                            port,
                            key_type: key_type.clone(),
                            fingerprint: fingerprint.clone(),
                            reply: tx,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(false);
                    }
                    match rx.await {
                        Ok(true) => {
                            known.add(crate::config::known_hosts::HostKey {
                                host,
                                port,
                                key_type,
                                fingerprint,
                            });
                            crate::config::known_hosts::save(&known)?;
                            Ok(true)
                        }
                        Ok(false) | Err(_) => Ok(false),
                    }
                }
            }
        }
    }
}

/// Try to load SSH private keys from standard locations.
fn find_private_keys() -> Vec<Arc<russh::keys::PrivateKey>> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    let ssh_dir = home.join(".ssh");
    let key_files = ["id_ed25519", "id_ecdsa", "id_rsa"];

    let mut keys = Vec::new();
    for name in &key_files {
        let path = ssh_dir.join(name);
        if path.exists() {
            // encrypted or unreadable keys: skip silently
            if let Ok(key) = load_secret_key(&path, None) {
                keys.push(Arc::new(key));
            }
        }
    }
    keys
}

/// Resolve the SSH login name. Prefer an explicit address-book username;
/// otherwise use `$USER` / `$USERNAME`, never `root`.
pub fn resolve_ssh_username(configured: Option<&str>) -> String {
    if let Some(u) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        return u.to_string();
    }
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into())
}

async fn send_disconnect_once(
    sent: &AtomicBool,
    event_tx: &mpsc::Sender<AppEvent>,
    connection_id: u64,
    reason: Option<String>,
) {
    if sent.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = event_tx
        .send(AppEvent::Disconnected {
            id: connection_id,
            reason,
        })
        .await;
}

#[allow(clippy::too_many_arguments)]
pub async fn connect_ssh(
    host: String,
    port: u16,
    username: Option<String>,
    cols: u16,
    rows: u16,
    connection_id: u64,
    event_tx: mpsc::Sender<AppEvent>,
    terminal_type: String,
    cancel: CancellationToken,
) {
    let result = connect_ssh_inner(
        &host,
        port,
        username.as_deref(),
        cols,
        rows,
        connection_id,
        &event_tx,
        &terminal_type,
        cancel,
    )
    .await;

    if let Err(e) = result {
        let _ = event_tx
            .send(AppEvent::Disconnected {
                id: connection_id,
                reason: Some(e.to_string()),
            })
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_ssh_inner(
    host: &str,
    port: u16,
    username: Option<&str>,
    cols: u16,
    rows: u16,
    connection_id: u64,
    event_tx: &mpsc::Sender<AppEvent>,
    terminal_type: &str,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let config = client::Config {
        ..Default::default()
    };

    let handler = SshHandler {
        host: host.to_string(),
        port,
        connection_id,
        event_tx: event_tx.clone(),
    };

    let addr = format!("{}:{}", host, port);
    let mut handle = tokio::select! {
        result = tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(Arc::new(config), addr, handler),
        ) => {
            match result {
                Ok(Ok(h)) => h,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "SSH connection to {}:{} timed out after {}s",
                        host,
                        port,
                        CONNECT_TIMEOUT.as_secs()
                    ));
                }
            }
        }
        _ = cancel.cancelled() => {
            return Err(anyhow::anyhow!("Connection cancelled"));
        }
    };

    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!("Connection cancelled"));
    }

    let user = resolve_ssh_username(username);

    // Try `none` auth first. BBSes and MUDs running embedded SSH servers
    // commonly accept `none` and handle login themselves in-channel after
    // the connection opens. OpenSSH's client does the same as its first
    // probe. If the server actually requires real auth this returns
    // failure and we fall through to keys → password.
    let mut authenticated = match handle.authenticate_none(&user).await {
        Ok(result) => result.success(),
        Err(_) => false,
    };

    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!("Connection cancelled"));
    }

    // Try key-based auth next
    if !authenticated {
        let keys = find_private_keys();
        for key in keys {
            let key_with_hash = PrivateKeyWithHashAlg::new(key, None);
            match handle.authenticate_publickey(&user, key_with_hash).await {
                Ok(result) if result.success() => {
                    authenticated = true;
                    break;
                }
                _ => continue,
            }
        }
    }

    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!("Connection cancelled"));
    }

    // If key auth failed, request password from UI
    if !authenticated {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = event_tx
            .send(AppEvent::PasswordNeeded {
                id: connection_id,
                reply: tx,
            })
            .await;

        match rx.await {
            Ok(password) => {
                // Wrap in Zeroizing so our copy of the buffer (originally
                // typed into the popup and moved through the oneshot) is
                // overwritten when this scope ends. russh::authenticate_password
                // takes ownership of its argument, so we hand it a clone — our
                // Zeroizing copy still wipes the original allocation on drop.
                let password = zeroize::Zeroizing::new(password);
                let result = handle
                    .authenticate_password(&user, password.as_str().to_owned())
                    .await?;
                if !result.success() {
                    return Err(anyhow::anyhow!("Authentication failed for user '{}'", user));
                }
            }
            Err(_) => {
                return Err(anyhow::anyhow!("Password prompt cancelled"));
            }
        }
    }

    // Open a session channel
    let channel = handle.channel_open_session().await?;

    // Request PTY
    channel
        .request_pty(true, terminal_type, cols as u32, rows as u32, 0, 0, &[])
        .await?;

    // Request shell
    channel.request_shell(true).await?;

    // Set up command channel
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ConnectionCommand>(64);

    let _ = event_tx
        .send(AppEvent::Connected {
            id: connection_id,
            cmd_tx,
            telnet_flags: None, // SSH doesn't use telnet flags
        })
        .await;

    // Split channel for concurrent read/write
    let (mut reader, writer) = channel.split();

    // At most one Disconnected for this session (previously reader + writer
    // each sent one and the second wiped capture-summary status).
    let disconnect_sent = Arc::new(AtomicBool::new(false));

    // Writer task
    let writer_cancel = cancel.clone();
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(ConnectionCommand::SendText(text)) => {
                            if writer.data(text.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        Some(ConnectionCommand::SendRaw(data)) => {
                            if writer.data(&data[..]).await.is_err() {
                                break;
                            }
                        }
                        Some(ConnectionCommand::Resize(new_cols, new_rows)) => {
                            let _ = writer
                                .window_change(new_cols as u32, new_rows as u32, 0, 0)
                                .await;
                        }
                        Some(ConnectionCommand::Disconnect) | None => {
                            let _ = writer.eof().await;
                            let _ = writer.close().await;
                            break;
                        }
                    }
                }
            }
        }
    });

    // Reader loop — owns the single Disconnected event when the peer closes.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = reader.wait() => {
                match msg {
                    Some(russh::ChannelMsg::Data { data }) => {
                        let _ = event_tx
                            .send(AppEvent::NetworkData {
                                id: connection_id,
                                data: data.to_vec(),
                            })
                            .await;
                    }
                    Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                        let _ = event_tx
                            .send(AppEvent::NetworkData {
                                id: connection_id,
                                data: data.to_vec(),
                            })
                            .await;
                    }
                    Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => {
                        break;
                    }
                    Some(_) => {}
                }
            }
        }
    }

    cancel.cancel();
    let _ = writer_handle.await;
    send_disconnect_once(&disconnect_sent, event_tx, connection_id, None).await;

    // Keep the session handle alive until both sides are done.
    drop(handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_uses_configured_username() {
        assert_eq!(resolve_ssh_username(Some("bbsuser")), "bbsuser");
        assert_eq!(resolve_ssh_username(Some("  alice  ")), "alice");
    }

    #[test]
    fn resolve_blank_or_none_is_not_root() {
        let u = resolve_ssh_username(None);
        assert_ne!(u, "root");
        assert!(!u.is_empty());
        let u2 = resolve_ssh_username(Some(""));
        assert_ne!(u2, "root");
        let u3 = resolve_ssh_username(Some("   "));
        assert_ne!(u3, "root");
    }
}
