# Contributing to NerdTerm

Thanks for your interest. This guide covers the dev setup, project layout, and conventions you'll want to know before touching code.

## Toolchain & build

Rust 1.85+ stable, edition 2024. `cargo` lives at `~/.cargo/bin/cargo` (rustup install) and is **not on the default PATH** for non-interactive shells. Prefix every cargo invocation:

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo <cmd>
```

(Works on both macOS and Linux.)

| Task | Command |
|---|---|
| Type-check (fast) | `cargo check` |
| Debug build | `cargo build` |
| Release build | `cargo build --release` |
| Lints | `cargo clippy -- -D warnings` |
| Format | `cargo fmt` |
| Tests | `cargo test` |

A clean `cargo check` from cold cache takes ~15s. Always run `cargo check` (not just `cargo build`) when iterating — it's faster.

## Manual testing

NerdTerm is a TUI — it calls `enable_raw_mode()`, switches to the alternate screen, and reads `crossterm::event::EventStream`. Running it from a non-TTY context (CI without a pty, automation harnesses, etc.) will either hang or corrupt the calling terminal. The TUI/event-loop layer is therefore not unit-tested; verify behavior interactively in a real terminal.

To verify behavior programmatically, write a unit test against the affected module (telnet filter, address-book IO, terminal emulator, capture file, known_hosts) — those don't need a live TTY.

## Repository layout

```
src/
├── main.rs              # tokio runtime, terminal setup, panic hook, event loop
├── app.rs               # App state machine, key handling, all enums (AppState, Protocol, Popup*)
├── events.rs            # AppEvent + ConnectionCommand enums (broken out to avoid circular deps)
├── config/
│   ├── address_book.rs  # TOML load/save + default entries
│   ├── capture.rs       # session capture file (open, header, append, O_EXCL)
│   └── known_hosts.rs   # SSH TOFU store: HostKey, Verdict, atomic save, quarantine
├── network/
│   ├── mod.rs           # connect_raw_tcp() — telnet over TCP
│   ├── telnet.rs        # IAC state machine, TelnetFlags (atomic shared with writer)
│   └── ssh.rs           # russh client, key-then-password auth, PTY + shell request
├── terminal/
│   └── emulator.rs      # vt100 parser wrapper with scrollback offset
└── ui/
    ├── mod.rs           # state-router dispatch to address_book / terminal_view
    ├── address_book.rs  # list view + status bar + menu bar
    ├── popup.rs         # Add/Edit/Delete/Password/HostKeyTrust/ChordHelp modals
    └── terminal_view.rs # PseudoTerminal widget + input bar (+ status bar)
```

There are no `tests/`, `examples/`, or `benches/` directories.

## Architecture in 60 seconds

- Single-threaded `tokio` runtime; one `App` owns all state, serialized through the main event loop in `main.rs`.
- `App` ↔ network task communicate over mpsc channels:
  - **inbound** (`AppEvent`): `Connected`, `NetworkData`, `Disconnected`, `PasswordNeeded`, `HostKeyTrustNeeded`, `HostKeyMismatch` — all tagged with a `connection_id`.
  - **outbound** (`ConnectionCommand`): `SendText`, `SendRaw`, `Resize`, `Disconnect`.
- `App.connection_id: u64` increments on every connect. Stale events from abandoned/aborted tasks carry an old id and are dropped — this is how we tolerate cancellation races. **If you add a new `AppEvent` variant, it must carry an `id: u64` and you must filter on it in the handler.**
- Telnet and SSH look identical to `App` post-`Connected` — the protocol-specific bits stay inside `network/`.
- Telnet negotiation state (`server_echo`, `naws_enabled`) is shared between the reader task (which sets it) and the app/writer (which reads it) via `Arc<TelnetFlags>` of atomics. `App.needs_local_echo()` consults this; SSH always returns `false` because the PTY echoes.
- `TerminalEmulator` wraps `vt100::Parser` with a 1000-line scrollback. To render scrollback you must call `apply_scroll()` before reading the screen and `reset_scroll_view()` after, otherwise incoming data lands in the wrong place. See `ui/terminal_view.rs` for the pattern.

## Library quirks worth knowing

- **vt100 0.16**: `set_size()` is on `Screen`, not `Parser`. Use `parser.screen_mut().set_size(rows, cols)`.
- **Version pinning**: `ratatui 0.30` + `vt100 0.16.2` + `tui-term 0.3.2` are a known-compatible set. Bumping any of them in isolation tends to break. (Cargo.toml uses caret ranges, but be wary.)
- **russh 0.57**: no separate `russh-keys` crate — `load_secret_key`, `PrivateKeyWithHashAlg`, `PublicKey` all live under `russh::keys`.
- **Config dir**: `dirs::config_dir()` resolves to `~/Library/Application Support` on macOS (not `~/.config`). Linux follows XDG.
- **crossterm 0.28 in basic mode** maps `Ctrl+]` (raw byte `0x1D`) to `KeyCode::Char('5')` + `CONTROL`, not `Char(']')`. Same for `Ctrl+\` / `Ctrl+^` / `Ctrl+_`. The chord trigger in `handle_key_connected` accepts both forms.

## Conventions

- All new errors propagate through `anyhow::Result`. Network tasks swallow errors and report them via `AppEvent::Disconnected { reason: Some(e.to_string()) }` rather than panicking.
- Background tasks are spawned with `tokio::spawn` and their `JoinHandle` is stored on `App.connection_handle` so `cancel_connection()` can `.abort()` them. Always pair a spawn with handle storage if the task represents a connection.
- Key handling is split by state: `handle_key_address_book`, `handle_key_connected` → `handle_key_line_buffered` / `handle_key_character`, `handle_key_popup`. Add new keys to the right one — don't sprinkle global keys at the top of `handle_key` unless they truly are global (only `Ctrl+C` is currently).
- Char mode emits ANSI escapes for arrow/function keys via `f_key_escape()` and the literal sequences in `handle_key_character`. If you add a key, match the xterm convention. Backspace/Delete sends `0x7F` (DEL), not `0x08` (BS) — most modern hosts and BBSes treat the former as "erase last char" and render the latter as `^H`.
- Filter `KeyEventKind::Release` at the top of `handle_crossterm_event`. Otherwise, terminals that deliver release events (kitty keyboard protocol etc.) double-fire handlers and silently consume chord state.

## Testing

Unit tests live inline as `#[cfg(test)] mod tests` blocks at the bottom of the modules they cover. Current coverage:

- `network::telnet` — IAC state machine (subneg edge cases, NAWS payload).
- `config::address_book` — load/save round-trip, corrupt-file quarantine, atomic-save invariants.
- `config::known_hosts` — verify verdicts, add+save round-trip, atomic-save, corrupt-file quarantine.
- `config::capture` — sanitize/path-collision helpers, file open with `O_EXCL`, header format, byte counting.
- `app::popup_tests` — `FormPopup` field navigation, validation, protocol toggle.
- `terminal::emulator` — `ScrollGuard` RAII (resets vt scrollback on drop).

The TUI/event-loop layer is not tested — see "Manual testing" above.

## Cutting a release

`scripts/release.sh` automates the pre-flight checks and the tag/push.

```bash
# Bump Cargo.toml version, regenerate Cargo.lock, commit, push:
$EDITOR Cargo.toml          # change `version = "0.1.0"` to `0.1.1`
cargo check                 # updates Cargo.lock to match
git add Cargo.toml Cargo.lock
git commit -m "Bump version to 0.1.1"
git push origin main

# Then run the release script:
scripts/release.sh 0.1.1
```

The script verifies you're on a clean `main` synced to `origin`, that `Cargo.toml` matches the requested version, that `cargo fmt` / `cargo clippy -D warnings` / `cargo test` / `cargo build --release` all pass, and that no obvious tokens or PEM private keys are present in tracked files. On success it prompts for confirmation, then creates an annotated `v0.1.1` tag and pushes it. The tag push triggers `.github/workflows/release.yml`, which builds for Linux x86_64 + macOS aarch64 + macOS x86_64 in parallel and attaches the binaries to the auto-created GitHub Release.

For a dry-run (checks only, no tag):

```bash
scripts/release.sh --check 0.1.1
```
