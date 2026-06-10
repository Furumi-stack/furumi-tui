# furumi_cli — Architecture

Cross-platform terminal client (cmus-style TUI) for the furumusic backend.
Targets: macOS, Linux (ALSA/Pulse/PipeWire), Windows (WASAPI, Windows Terminal).

## 1. Technology choices

### TUI: ratatui 0.30 + crossterm 0.29

Evaluated: **ratatui**, cursive, tui-realm, iocraft.

- **ratatui 0.30.x** — the de-facto standard (gitui, yazi, spotify-player all use it).
  0.30 split the project into workspace crates (`ratatui-core`, `ratatui-widgets`,
  `ratatui-crossterm`) with a stable core API. Stock widgets cover everything we
  need: `Tabs`, `List`, `Table`, nested `Layout` for tile grids.
- cursive — maintenance mode since 2024, rejected.
- tui-realm — viable framework on top of ratatui (termusic uses it), but a
  single-maintainer abstraction layer; we prefer plain ratatui with our own
  thin component layer.
- iocraft — too young, optimized for inline CLI output rather than fullscreen apps.
- termion — Unix-only, eliminated (we need Windows).

crossterm is the only backend that covers Windows. Caveats to handle:
- Enable kitty keyboard enhancement flags only when
  `supports_keyboard_enhancement()` returns true; always pop flags on exit.
- Filter key events to `KeyEventKind::Press` (Windows and kitty-enhanced
  terminals also deliver Repeat/Release — otherwise bindings double-fire).
- Restore the terminal on panic (panic hook) — a TUI that corrupts the shell
  is the #1 reliability complaint.

### Keybindings: crokey + TOML keymap

- **crokey 1.4** — parses/formats key combos (`ctrl-a`, `g`), serde support, used
  for the config file format.
- Keymap model copied from spotify-player: a `[[keymaps]]` TOML table mapping a
  *key sequence* (space-separated chords, e.g. `"g g"`, `"C-c x"`) to a
  `Command` enum, optionally parameterized (`{ SeekForward = { seconds = 10 } }`).
- A small chord state machine resolves sequences; bindings are layered:
  built-in defaults ← user config (`~/.config/furumi/keymap.toml`).
- Bindings resolve per *input context* (Global, LibraryGrid, TrackList,
  TextInput, Popup) so the same key can mean different things per view.

### Audio: rodio 0.22 + stream-download, behind a backend trait

Evaluated: **rodio**, kira, raw cpal+symphonia, gstreamer-rs, libmpv.

- **rodio 0.22** (`Player` / `DeviceSinkBuilder` API — note the 0.21/0.22 renames;
  most older tutorials are outdated). Symphonia is the default decoder; enable
  the `aac`, `isomp4`, `alac` features for m4a support. Pure Rust → trivial
  cross-compilation; cpal covers CoreAudio / ALSA / WASAPI.
- **stream-download 0.24** bridges HTTP to rodio: background download exposing
  blocking `Read + Seek`, built on reqwest (shares our authenticated client,
  auth headers included), seek into undownloaded regions via HTTP Range
  (the backend's `/stream/{id}` supports Range), temp-file storage, retries.
- kira — game-audio oriented, no network story, rejected.
- gstreamer / libmpv — best playback quality but heavy system dependencies;
  not acceptable as the only backend for a portable CLI.

Playback lives behind a trait so backends can be added later (termusic ships
rodio + mpv + gstreamer this way):

```rust
trait AudioBackend {
    fn play(&mut self, source: TrackSource) -> Result<()>;
    fn pause(&mut self); fn resume(&mut self);
    fn seek(&mut self, pos: Duration) -> Result<()>;
    fn set_volume(&mut self, v: f32);
    fn position(&self) -> Duration;
    fn events(&self) -> Receiver<PlayerEvent>; // TrackEnded, Failed, ...
}
```

Gapless-ish playback: pre-open the `stream-download` source and decoder for the
next queue item and append it to the rodio `Player` before the current track
ends. (True gapless is impossible for AAC/M4A anyway — symphonia has no AAC
gapless trim.)

### Async runtime: tokio

Needed for: crossterm `EventStream`, reqwest, stream-download, device-sync
polling, debounced search. The audio decode thread is rodio's own; everything
else is async tasks talking over channels.

## 2. Application architecture

Elm-style (TEA) core with a component-per-view UI layer — the pattern from the
official ratatui component template and spotify-player.

```
                 ┌────────────────────────────────────────────┐
                 │                  main loop                  │
                 │  recv Event -> keymap -> Action -> update() │
                 │  tick -> draw(&state)                       │
                 └───────▲──────────────────────────┬──────────┘
        Event (mpsc)     │                          │ Command (spawn task / send msg)
   ┌─────────────────────┼──────────────┐           │
   │ terminal input (crossterm stream)  │   ┌───────▼────────┐
   │ api task results                   │   │  side effects  │
   │ player events (TrackEnded, ...)    │   │ api::Client    │
   │ device-sync poll results/commands  │   │ player::Engine │
   │ tick (render + position updates)   │   │ sync::Poller   │
   └────────────────────────────────────┘   └────────────────┘
```

Key rules:

- **Single source of truth**: one `AppState` struct, mutated only in `update()`.
  Views are pure render functions over `&AppState`.
- **No blocking in the UI loop.** All I/O (HTTP, audio open) happens in spawned
  tasks that report back via the event channel. Every remote list is a
  `Loadable<T> { NotAsked, Loading, Loaded(T), Failed(Error) }` so views can
  render spinners and errors honestly.
- **Input → Action indirection**: raw key events are translated by the keymap
  into semantic `Action`s (`PlayPause`, `FocusNextTab`, `Select`, `Back`,
  `SeekForward(10)`). Views never see raw keys; this is what makes bindings
  configurable and the app testable.

### Module layout (single crate now, splittable later)

```
src/
  main.rs            // setup: terminal guard, tokio, channels, run loop
  config/            // Config + keymap loading (figment or manual TOML merge)
  api/               // typed client for /api/player/*
    client.rs        //   reqwest wrapper: base_url, bearer auth, retries
    auth.rs          //   password login, token store, auto-refresh (15min TTL)
    models.rs        //   ArtistCard, Release, TrackItem, PlaylistCard, ...
  player/            // playback engine
    backend.rs       //   AudioBackend trait
    rodio_backend.rs //   rodio Player + stream-download sources
    queue.rs         //   queue, shuffle, repeat_mode, next-track prefetch
  sync/              // connected devices: heartbeat/poll loop, command handling
  app/               // AppState, Action, Event, update()
  ui/                // ratatui rendering
    views/           //   library_grid, artist, release, playlists, search,
                     //   queue, devices, now_playing bar, popups
    theme.rs
```

The `api`, `player`, and `app` layers do not import `ui` or ratatui. If a
shared core for furumi_macos/android ever makes sense, those modules extract
into workspace crates without surgery.

## 3. UI model

Persistent layout: a tab bar on top, the active view in the middle, a
now-playing/status bar at the bottom (track, position gauge, volume, shuffle/
repeat, active device indicator).

Tabs (each owns a navigation stack, like a browser per tab):

1. **Library** — paginated grid of artist tiles (`GET /artists`).
   `Enter` on a tile pushes **Artist view** (`GET /artists/{id}`: metadata,
   top tracks, releases list). Selecting a release pushes **Release view**
   (`GET /releases/{id}`: metadata + track list). `Esc`/`Backspace` pops.
2. **Search** — debounced `GET /search?q=` with artists/releases/tracks sections.
3. **Playlists** — own + saved playlists, likes ("Liked tracks" virtual playlist).
4. **Queue** — current play queue, reorder/remove.
5. **Devices** — connected devices list, pick active device, transfer playback.

Navigation state is `Vec<Route>` per tab; a `Route` is an enum
(`ArtistGrid { page }`, `Artist { id }`, `Release { id }`, ...). Views cache
their loaded data in `AppState` keyed by route so Back is instant.

Tile grid: computed from terminal width (`Layout` columns × rows), each tile a
bordered block with artist name (cover art rendering in-terminal is a later,
optional feature — e.g. ratatui-image with kitty/sixel detection, never a
hard dependency).

## 4. Backend integration notes

(Verified against the furumusic source; base path `/api/player`.)

- **Auth**: `POST /api/auth/password` → access token (15 min) + refresh token
  (60 days). Client stores tokens at `~/.config/furumi/credentials.json`
  (0600) and refreshes proactively via `POST /api/auth/refresh`. All API calls
  go through one client that retries once on 401 after refreshing.
- **Streaming**: `GET /api/player/stream/{track_id}` with `Accept-Ranges:
  bytes` — exactly what stream-download needs for seek. Original files are
  served untranscoded (mp3/flac/ogg/m4a/...), hence the symphonia feature set.
- **Playback state**: persisted server-side via `PUT /api/player/state`
  (queue, position, shuffle, repeat, volume). We push throttled updates
  (on track change + every ~10s while playing) and restore on startup.
- **History/scrobbling**: `POST /history` on track completion;
  `POST /lastfm/now-playing` and `/lastfm/scrobble` if last.fm is connected.
- **Connected devices**: *polling, not websockets.* The sync task:
  - sends `POST /devices/poll` every ~5s while the app runs (device TTL is
    30s; commands TTL 20s) with our stable `device_id` (generated once,
    persisted) and current `playback_state`;
  - applies returned commands (`transfer_state` → load queue/position and
    start/stop locally; play/pause/seek commands when we are the active
    device but controlled remotely);
  - feeds the device list into the Devices tab. Activating another device =
    `POST /devices/active`; we then stop local audio and become a remote
    control (UI keeps working, actions are sent via `POST /devices/command`).
- **Jams** (collaborative sessions) exist in the API — out of scope for v1,
  but the sync task's command-handling design must not preclude them.

## 5. Reliability checklist

- Terminal guard type + panic hook: raw mode/alternate screen/keyboard flags
  always restored, even on panic.
- Every spawned task's failure becomes an `Event::TaskFailed` rendered as a
  status-bar error — no silent hangs, no `unwrap` on I/O.
- Token refresh races guarded by a single-flight lock.
- Audio device disappearance (headphones unplugged) → backend emits
  `PlayerEvent::Failed`, engine retries on default device, pauses on repeated
  failure.
- Config/keymap parse errors are reported with line context and fall back to
  defaults — a typo in keymap.toml must not brick the app.

## 6. Suggested initial dependencies

```toml
[dependencies]
ratatui = "0.30"
crossterm = "0.29"
crokey = "1.4"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time"] }
rodio = { version = "0.22", features = ["symphonia-aac", "symphonia-isomp4", "symphonia-alac"] } # check exact feature names
stream-download = { version = "0.24", features = ["reqwest"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
thiserror = "2"
anyhow = "1"
directories = "6"      # config/cache paths per-OS
tracing = "0.1"        # file-based logging (never stdout — it's the UI)
tracing-subscriber = "0.3"
```

## 7. Build order (milestones)

1. Skeleton: terminal guard, event loop, tab bar, status bar, keymap with defaults.
2. `api` crate-module: auth + artists/releases/tracks; Library grid → Artist → Release navigation.
3. Playback: rodio backend + stream-download, queue, now-playing bar, seek/volume.
4. Likes, playlists, search, history reporting.
5. Device sync: heartbeat/poll, transfer playback, remote-control mode.
6. Polish: server-side state restore, last.fm, config file, themes, optional cover art.
