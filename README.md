# furumi

![furumi TUI screenshot](furumi.png)

`furumi` is a cross-platform terminal client for a furumusic server. It
provides a fast TUI for browsing the library, playing music, managing the
queue and playlists, controlling devices, and inspecting logs without leaving
the terminal.

## Features

- Browse the full artist library in tile or table view.
- Open artist pages, releases, and track lists from inside the TUI.
- Search artists, releases, and tracks with `/`.
- Play local audio with seek, volume, shuffle, repeat, and like controls.
- Add tracks next in queue, append them to the queue, or clear the queue.
- Browse playlists, liked tracks, and add tracks to playlists.
- Pick the active playback device and control remote devices.
- Use OS media keys through MPRIS/system media controls.
- Inspect live in-app logs and a persistent log file.
- Customize key bindings with a TOML keymap.

## Installation

Requires Rust 1.88+.

```bash
cargo build --release
./target/release/furumi
```

The release binary is named `furumi`:

```bash
cargo run --release --bin furumi
```

### Linux

Audio output needs the system ALSA library. PipeWire and PulseAudio are used
through the ALSA compatibility layer at runtime.

```bash
# Debian / Ubuntu
sudo apt install libasound2-dev pkg-config

# Fedora
sudo dnf install alsa-lib-devel pkgconf-pkg-config

# Arch
sudo pacman -S alsa-lib pkgconf
```

Everything else is handled by Rust dependencies: TLS uses `rustls`, MPRIS uses
`zbus`, and image/audio decoding is provided by Rust crates.

### macOS and Windows

No extra system packages are required.

## First Run

On startup, `furumi` opens the login screen:

1. Enter your furumusic server URL.
2. Sign in with username/password or SSO.
3. After a successful login, the session is saved locally.

The SSO flow opens your browser automatically. If the loopback callback is not
available, `furumi` shows the URL and accepts either a pasted `furumi://...`
callback link or the short `furu_mx_...` code.

## Controls

Common key bindings:

| Key | Action |
| --- | --- |
| `?` | Show key binding help |
| `q`, `Ctrl-C` | Quit |
| `Tab`, `Shift-Tab` | Next / previous tab |
| `1`...`4` | Jump to a tab |
| `j` / `k`, arrows | Move down / up |
| `h` / `l`, arrows | Move left / right |
| `Enter` | Open or select item |
| `Esc`, `Backspace` | Go back |
| `Space` | Play / pause |
| `n`, `p` | Next / previous track |
| `.`, `,` | Seek 10 seconds forward / backward |
| `+`, `-` | Volume up / down |
| `s` | Toggle shuffle |
| `r` | Cycle repeat mode |
| `x` | Like / unlike |
| `a` | Add track next |
| `Shift-A` | Add track to the end of the queue |
| `Shift-P` | Add track to a playlist |
| `Shift-D` | Open device picker |
| `v` | Toggle tile/table view |
| `/` | Search |
| `:` | Open command line |

Command line examples:

```text
:q
:logout
:volume 40
:seek +30
:seek -10
:seek 1:30
:shuffle
:repeat off
:repeat one
:repeat all
:clear
:next
:prev
:play
:pause
:devices
:logs debug
```

## Configuration

`furumi` stores configuration in the platform app config directory:

- Linux: `~/.config/furumi`
- macOS: `~/Library/Application Support/furumi`
- Windows: `%APPDATA%\furumi`

Important files:

- `credentials.json` - saved login session. On Unix it is written with `0600`
  permissions.
- `device_id` - stable identifier for this TUI client during device sync.
- `keymap.toml` - user key binding overrides.

See [`src/config/default_keymap.toml`](src/config/default_keymap.toml) for the
default format. Example:

```toml
[[keymaps]]
key_sequence = "ctrl-n"
command = "NextTrack"

[[keymaps]]
key_sequence = "ctrl-f"
command = { SeekForward = { seconds = 30 } }
```

A user binding replaces the default binding with the same key sequence and
context.

## Logs

The Logs tab shows a live in-memory ring buffer inside the TUI. You can jump
to it and set the level filter with:

```text
:logs error
:logs warn
:logs info
:logs debug
:logs trace
```

The persistent log file is written to the platform cache directory as
`furumi-cli.log`. File logging is filtered by `RUST_LOG`:

```bash
RUST_LOG=furumi_tui=debug cargo run --release --bin furumi
```

## Architecture

At a glance:

- UI: `ratatui` + `crossterm`.
- Runtime: `tokio`.
- HTTP: `reqwest` + `rustls`.
- Audio: `rodio` + `stream-download`.
- Keymap config: `crokey` + TOML.
- State model: one `AppState`, events, and an update loop.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for more detail.
