# furumi

Terminal client (TUI) for the furumusic server. Cross-platform: Linux,
macOS, Windows.

## Building

Rust 1.88+ (edition 2024).

```bash
cargo build --release      # binary: target/release/furumi
```

### Linux

Sound output needs the system ALSA library — the one and only system
build dependency (PipeWire/PulseAudio are reached through the ALSA
compatibility layer at runtime):

```bash
# Debian/Ubuntu
sudo apt install libasound2-dev pkg-config
# Fedora
sudo dnf install alsa-lib-devel pkgconf-pkg-config
# Arch
sudo pacman -S alsa-lib pkgconf
```

Everything else is pure Rust: TLS is rustls, MPRIS media keys go through
zbus (no libdbus), images and audio decoding are Rust crates.

### macOS / Windows

No system packages required. (Windows: media keys are not wired up yet;
playback works.)

## Configuration

- `~/.config/furumi/keymap.toml` — keybinding overrides, see
  `src/config/default_keymap.toml` for the format and defaults.
- `~/.config/furumi/credentials.json` — created on login (0600).
- Logs: in-app on the Logs tab (`5`), and in the cache dir
  (`furumi-cli.log`), filtered by `RUST_LOG`.
