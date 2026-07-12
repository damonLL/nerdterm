pub mod ssh;
pub mod telnet;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::{AppEvent, ConnectionCommand};
use telnet::{TelnetFilter, TelnetFlags};

/// How long to wait for the TCP handshake before giving up.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn a TCP connection with telnet protocol handling.
/// All events are tagged with `connection_id` so the app can ignore stale ones.
/// `cancel` tears down the reader and writer together when the user disconnects.
#[allow(clippy::too_many_arguments)]
pub async fn connect_raw_tcp(
    host: String,
    port: u16,
    cols: u16,
    rows: u16,
    connection_id: u64,
    event_tx: mpsc::Sender<AppEvent>,
    terminal_type: String,
    cancel: CancellationToken,
) {
    let addr = format!("{}:{}", host, port);
    let stream = tokio::select! {
        result = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)) => {
            match result {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    let _ = event_tx
                        .send(AppEvent::Disconnected {
                            id: connection_id,
                            reason: Some(e.to_string()),
                        })
                        .await;
                    return;
                }
                Err(_) => {
                    let _ = event_tx
                        .send(AppEvent::Disconnected {
                            id: connection_id,
                            reason: Some(format!(
                                "Connection to {}:{} timed out after {}s",
                                host,
                                port,
                                CONNECT_TIMEOUT.as_secs()
                            )),
                        })
                        .await;
                    return;
                }
            }
        }
        _ = cancel.cancelled() => return,
    };

    // Disable Nagle's algorithm for interactive use. Without this,
    // single-keystroke writes get coalesced for up to ~40 ms each,
    // which feels exactly like dropped/delayed input during fast
    // typing (e.g. password entry on a BBS). System `telnet` does
    // the same thing.
    let _ = stream.set_nodelay(true);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ConnectionCommand>(64);
    let (mut reader, mut writer) = stream.into_split();

    // Shared telnet negotiation state
    let flags = Arc::new(TelnetFlags::new());

    let _ = event_tx
        .send(AppEvent::Connected {
            id: connection_id,
            cmd_tx,
            telnet_flags: Some(flags.clone()),
        })
        .await;

    // Channel for the reader to send telnet responses to the writer
    let (telnet_tx, mut telnet_rx) = mpsc::channel::<Vec<u8>>(64);

    // Reader task — cancelled with the shared token so cancel_connection
    // does not leave a half-open socket/FD behind.
    let tx = event_tx.clone();
    let id = connection_id;
    let reader_flags = flags.clone();
    let reader_cancel = cancel.clone();
    let reader_handle = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut filter = TelnetFilter::new(cols, rows, reader_flags, terminal_type);
        loop {
            tokio::select! {
                _ = reader_cancel.cancelled() => break,
                result = reader.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            let _ = tx
                                .send(AppEvent::Disconnected { id, reason: None })
                                .await;
                            break;
                        }
                        Ok(n) => {
                            let output = filter.process(&buf[..n]);
                            if !output.response.is_empty() {
                                let _ = telnet_tx.send(output.response).await;
                            }
                            if !output.data.is_empty() {
                                let _ = tx
                                    .send(AppEvent::NetworkData {
                                        id,
                                        data: output.data,
                                    })
                                    .await;
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(AppEvent::Disconnected {
                                    id,
                                    reason: Some(e.to_string()),
                                })
                                .await;
                            break;
                        }
                    }
                }
            }
        }
    });

    // Writer loop
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ConnectionCommand::SendText(text) => {
                        if writer.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    ConnectionCommand::SendRaw(data) => {
                        if writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    ConnectionCommand::Resize(cols, rows) => {
                        flags.cols.store(cols, Ordering::Relaxed);
                        flags.rows.store(rows, Ordering::Relaxed);
                        if flags.naws_enabled.load(Ordering::Relaxed) {
                            let naws = telnet::build_naws(cols, rows);
                            let _ = writer.write_all(&naws).await;
                        }
                    }
                    ConnectionCommand::Disconnect => break,
                }
            }
            Some(data) = telnet_rx.recv() => {
                if writer.write_all(&data).await.is_err() {
                    break;
                }
            }
            else => break,
        }
    }

    // Tear down peer half and wait for the reader so the socket is fully closed.
    cancel.cancel();
    drop(writer);
    let _ = reader_handle.await;
}
