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

## Download

Pre-built binaries for the current release live on the [Releases page](https://github.com/damonLL/nerdterm/releases/latest). Pick the tarball for your platform:

| Platform | File |
|---|---|
| Linux (x86_64) | `nerdterm-x86_64-unknown-linux-gnu.tar.gz` |
| macOS (Apple Silicon) | `nerdterm-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `nerdterm-x86_64-apple-darwin.tar.gz` |

Extract and run:

```bash
tar -xzf nerdterm-<target>.tar.gz
./nerdterm
```

Optionally drop the binary into your `PATH` (`~/.local/bin`, `/usr/local/bin`, etc.).

Each tarball ships with a `.sha256` companion. Verify before running:

```bash
shasum -a 256 -c nerdterm-<target>.tar.gz.sha256
```

**macOS Gatekeeper:** the binaries are unsigned. The first launch will be blocked — either right-click the binary in Finder → *Open* and confirm, or strip the quarantine attribute from the terminal:

```bash
xattr -d com.apple.quarantine nerdterm
```

## Build from source

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
| `S` | Open settings popup |
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

First launch ships with a few public hosts you can connect to without configuration: CoffeeMUD, a Star Wars ASCII stream, Aardwolf MUD, Legend of the Red Dragon, and the Synchronet BBS.

## Settings

Press `S` from the address book to open the settings popup. Three knobs are exposed:

| Setting | Description | Default |
|---|---|---|
| Scrollback (lines) | Off-screen history depth per session | `1000` |
| Default input mode | Initial input mode for new connections (`line` or `character`) | `line` |
| Terminal type | Reported during telnet/SSH negotiation (e.g. `xterm-256color`, `ANSI`) | `xterm-256color` |

Settings persist to `<config_dir>/nerdterm/settings.toml` (`~/.config/nerdterm/settings.toml` on Linux, `~/Library/Application Support/nerdterm/settings.toml` on macOS) and apply to the *next* connection. The active session and any suspended session keep the values they were started with.

If the file becomes unparseable (e.g. hand-edited badly), nerdterm renames it to `settings.toml.corrupt-<timestamp>` on startup, falls back to defaults, and surfaces a warning on the address-book status line.

## Status

Telnet, SSH (key + password + `known_hosts` TOFU), address book persistence, session suspend, and session capture all ship. Multi-tab sessions are planned for a future v2.
