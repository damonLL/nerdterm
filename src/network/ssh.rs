use std::sync::Arc;

use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};
use tokio::sync::mpsc;

use crate::events::{AppEvent, ConnectionCommand};

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
    let mut handle =
        client::connect(Arc::new(config), format!("{}:{}", host, port), handler).await?;

    let user = username.unwrap_or("root");

    // Try key-based auth first
    let mut authenticated = false;
    let keys = find_private_keys();
    for key in keys {
        let key_with_hash = PrivateKeyWithHashAlg::new(key, None);
        match handle.authenticate_publickey(user, key_with_hash).await {
            Ok(result) if result.success() => {
                authenticated = true;
                break;
            }
            _ => continue,
        }
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
                    .authenticate_password(user, password.as_str().to_owned())
                    .await?;
                if !result.success() {
                    return Err(anyhow::anyhow!("Authentication failed"));
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

    // Writer task
    let writer_event_tx = event_tx.clone();
    let writer_id = connection_id;
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                ConnectionCommand::SendText(text) => {
                    if writer.data(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                ConnectionCommand::SendRaw(data) => {
                    if writer.data(&data[..]).await.is_err() {
                        break;
                    }
                }
                ConnectionCommand::Resize(new_cols, new_rows) => {
                    let _ = writer
                        .window_change(new_cols as u32, new_rows as u32, 0, 0)
                        .await;
                }
                ConnectionCommand::Disconnect => {
                    let _ = writer.eof().await;
                    let _ = writer.close().await;
                    break;
                }
            }
        }
        let _ = writer_event_tx
            .send(AppEvent::Disconnected {
                id: writer_id,
                reason: None,
            })
            .await;
    });

    // Reader loop
    while let Some(msg) = reader.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => {
                let _ = event_tx
                    .send(AppEvent::NetworkData {
                        id: connection_id,
                        data: data.to_vec(),
                    })
                    .await;
            }
            russh::ChannelMsg::ExtendedData { data, .. } => {
                let _ = event_tx
                    .send(AppEvent::NetworkData {
                        id: connection_id,
                        data: data.to_vec(),
                    })
                    .await;
            }
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => {
                break;
            }
            _ => {}
        }
    }

    let _ = event_tx
        .send(AppEvent::Disconnected {
            id: connection_id,
            reason: None,
        })
        .await;

    Ok(())
}
