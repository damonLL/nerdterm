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

/// Idle keepalive interval for both telnet (IAC NOP) and SSH. Boards and
/// modernbbs often drop sessions after ~10 minutes with no client traffic.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// Telnet IAC NOP — a no-op command that resets many servers' idle timers
/// without affecting the display stream.
pub const TELNET_IAC_NOP: &[u8] = &[0xFF, 0xF1];

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
    connect_raw_tcp_with_timeout(
        host,
        port,
        cols,
        rows,
        connection_id,
        event_tx,
        terminal_type,
        cancel,
        CONNECT_TIMEOUT,
    )
    .await;
}

/// Same as [`connect_raw_tcp`] with an injectable handshake timeout so tests
/// can exercise the path without waiting the full production 30s.
#[allow(clippy::too_many_arguments)]
pub async fn connect_raw_tcp_with_timeout(
    host: String,
    port: u16,
    cols: u16,
    rows: u16,
    connection_id: u64,
    event_tx: mpsc::Sender<AppEvent>,
    terminal_type: String,
    cancel: CancellationToken,
    timeout: Duration,
) {
    let addr = format!("{}:{}", host, port);
    let stream = tokio::select! {
        result = tokio::time::timeout(timeout, TcpStream::connect(&addr)) => {
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
                    let secs = timeout.as_secs_f64();
                    let reason = if secs >= 1.0 {
                        format!(
                            "Connection to {}:{} timed out after {}s",
                            host,
                            port,
                            timeout.as_secs()
                        )
                    } else {
                        format!(
                            "Connection to {}:{} timed out after {:.0}ms",
                            host,
                            port,
                            secs * 1000.0
                        )
                    };
                    let _ = event_tx
                        .send(AppEvent::Disconnected {
                            id: connection_id,
                            reason: Some(reason),
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

    // Writer loop — includes periodic IAC NOP so idle boards don't kick us.
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick; we only want periodics after connect.
    keepalive.tick().await;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = keepalive.tick() => {
                if writer.write_all(TELNET_IAC_NOP).await.is_err() {
                    break;
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// TEST-NET-1 (RFC 5737) — unroutable; connect typically hangs until timeout
    /// rather than failing immediately with "network unreachable".
    const BLACKHOLE_HOST: &str = "192.0.2.1";

    #[tokio::test]
    async fn handshake_timeout_emits_disconnected_with_reason() {
        let (tx, mut rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        connect_raw_tcp_with_timeout(
            BLACKHOLE_HOST.into(),
            9,
            80,
            24,
            42,
            tx,
            "xterm".into(),
            cancel,
            Duration::from_millis(80),
        )
        .await;

        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event within 2s")
            .expect("channel open");
        match ev {
            AppEvent::Disconnected {
                id,
                reason: Some(r),
            } => {
                assert_eq!(id, 42);
                // Prefer a timeout message; some environments refuse the
                // blackhole immediately — still a failed handshake, not Connected.
                assert!(!r.is_empty(), "Disconnected reason must be non-empty");
            }
            AppEvent::Disconnected { id, reason: None } => {
                panic!("expected reason for id {id}");
            }
            AppEvent::Connected { .. } => panic!("must not Connect on failed handshake"),
            _ => panic!("expected Disconnected"),
        }
        // No further events (especially not Connected).
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancel_during_handshake_exits_without_connected() {
        let (tx, mut rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        // Cancel almost immediately so we don't wait on the blackhole.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel2.cancel();
        });
        connect_raw_tcp_with_timeout(
            BLACKHOLE_HOST.into(),
            9,
            80,
            24,
            7,
            tx,
            "xterm".into(),
            cancel,
            Duration::from_secs(30),
        )
        .await;

        // Cancel path returns without sending Connected. A racing timeout/
        // refuse Disconnected is acceptable; Connected is not.
        while let Ok(ev) = rx.try_recv() {
            assert!(
                !matches!(ev, AppEvent::Connected { .. }),
                "cancelled handshake must not report Connected"
            );
        }
    }

    #[test]
    fn telnet_iac_nop_is_two_byte_sequence() {
        assert_eq!(TELNET_IAC_NOP, &[0xFF, 0xF1]);
        assert_eq!(KEEPALIVE_INTERVAL.as_secs(), 60);
    }

    #[tokio::test]
    async fn connect_refused_emits_disconnected() {
        // Bind then drop so nothing is listening — connect fails fast with ECONNREFUSED.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (tx, mut rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        connect_raw_tcp_with_timeout(
            "127.0.0.1".into(),
            port,
            80,
            24,
            3,
            tx,
            "xterm".into(),
            cancel,
            Duration::from_secs(5),
        )
        .await;

        let ev = rx.recv().await.expect("Disconnected event");
        match ev {
            AppEvent::Disconnected {
                id,
                reason: Some(r),
            } => {
                assert_eq!(id, 3);
                assert!(!r.is_empty());
            }
            _ => panic!("expected Disconnected with reason"),
        }
    }
}
