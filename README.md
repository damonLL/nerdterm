# NerdTerm

A cross-platform terminal client for BBS, MUD, and SSH connections, written in Rust. Built around an address book of saved hosts, with a real VT/xterm-compatible emulator so colored ANSI art and full-screen apps render correctly.

## Features

- **Telnet** with proper IAC negotiation (ECHO, SUPPRESS-GO-AHEAD, NAWS auto-resize, TERMINAL-TYPE → `XTERM-256COLOR`).
- **SSH** via [russh](https://crates.io/crates/russh): key-based auth from `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`, falling back to a masked password prompt. Host keys are verified against a TOFU `known_hosts` store with an explicit trust prompt on first connect and a hard-fail prompt on key mismatch.
- **Address book** persisted as TOML (`~/Library/Application Support/nerdterm/address_book.toml` on macOS, `$XDG_CONFIG_HOME/nerdterm/` on Linux). Add / edit / delete entries from the UI.
- **Two input modes** when connected — line-buffered (with command history, good for MUDs) and character-at-a-time (raw keys + ANSI escapes, good for full-screen apps like vim or BBS doors).
- **Scrollback buffer** of 1000 lines.
- **Session suspend**: `Esc` returns to the address book without dropping the connection; resume with `Enter` on the highlighted entry.
- **Session capture**: hotkey-toggled raw-byte transcript of inbound server data to a self-describing `.log` file in the user's config dir. Activate with `Ctrl+] l`; help via `Ctrl+] ?`. ANSI is preserved so `cat` replays the session in color.

## Install

Requires a Rust toolchain (1.85+).

```bash
git clone https://github.com/damonLL/nerdterm.git
cd nerdterm
cargo build --release
./target/release/nerdterm
```

Or run directly with `cargo run --release`.

## Keybindings

### Address book

| Key | Action |
|---|---|
| `↑`/`↓` or `k`/`j` | Move selection |
| `Enter` | Connect (or resume if already connected) |
| `A` | Add entry |
| `E` | Edit selected entry |
| `D` | Delete selected entry |
| `Ctrl+D` | Disconnect active session |
| `Q` / `Esc` | Quit (disconnects first) |

### Connected session

| Key | Action |
|---|---|
| `Tab` | Toggle line-buffered ↔ character mode |
| `Esc` | Suspend session, return to address book |
| `Ctrl+D` | Disconnect |
| `Shift+PgUp` / `Shift+PgDn` | Scroll back/forward through scrollback |

In line mode: `↑`/`↓` browse command history, `Enter` sends the line.
In character mode: arrows, function keys, and `Ctrl+<x>` are sent raw.

### Edit/Add popup

`Tab`/`Shift+Tab` move between fields. `Space` toggles Telnet/SSH (port auto-flips between 23 and 22). `Enter` saves, `Esc` cancels.

## Default entries

First launch ships with a few public hosts you can connect to without configuration: a Star Wars ASCII stream, Aardwolf MUD, Legend of the Red Dragon, and the Synchronet BBS.

## Status

Telnet, SSH (key + password + `known_hosts` TOFU), address book persistence, session suspend, and session capture all ship. Multi-tab sessions are planned for a future v2.
