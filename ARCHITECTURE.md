# furumi architecture

`furumi` is a single Rust binary organized around an Elm-style state/update
loop. The UI state remains synchronous and deterministic; filesystem, SQLite,
audio, networking, artwork, and media-control work is performed by runtime
services and reported back as application events.

## Runtime flow

```text
terminal/media/player/network events
                 |
                 v
          app::event::AppEvent
                 |
                 v
       input/keymap -> Action
                 |
                 v
           app::update()
        mutates AppState and
        requests an Effect
                 |
                 v
       app runtime performs I/O
                 |
                 +----> new AppEvent
```

`AppState` is the single UI source of truth. Rendering modules receive shared
state and do not own background tasks. Blocking database and file operations
run outside the terminal event loop.

## Module layout

```text
src/
  main.rs             process, terminal, Tokio, and OS-media setup
  app/
    state.rs          UI and navigation state
    action.rs         semantic user actions
    event.rs          runtime-to-UI events
    update.rs         pure state transitions and requested effects
    update_tests.rs   update/selection/queue behavior tests
    mod.rs            runtime orchestration and effect execution
    popup.rs          popup submission behavior
    input.rs          editable text input
    command.rs        command model
    cmdline.rs        command-line execution
  library/
    mod.rs            SQLite-backed library operations
    import.rs         tags, audio metadata, and directory import
    models.rs         library-facing data types
    tests.rs          library integration tests
  player/
    mod.rs            rodio playback controller
    analyzer.rs       visualization audio analysis
  federation/
    mod.rs            DHT manager, search, downloads, and caching
    catalog.rs        peer catalog protocol and merge logic
    audio.rs          peer audio transport
    tests.rs          federation ranking/appearance tests
  devices/
    tests.rs          trusted-device sync tests
  devices.rs          trusted-device operation log and wire protocol
  ui/                 ratatui rendering by screen
  config/             settings, logging, and keymaps
  media.rs            platform media-key/now-playing integration
  visualizer.rs       Rhai visualization host
  visualizations/     bundled Rhai scripts
  art.rs              image decode and terminal-cell preparation
  share.rs            share-link parsing and generation
  streaming.rs        growing-file reader used during downloads
```

Large orchestration modules are intentionally separated from their tests.
When they are split further, boundaries should follow services rather than
line count: playback coordination, network-library maintenance, device
storage, and device transport are the natural seams.

## Local library

`library::Library` owns a mutex-protected SQLite connection. It is the only
layer that issues library SQL and returns typed models to the rest of the
application. The schema covers artists, releases, tracks, artist relations,
playlists, likes, playback history, federated pending tracks, and cached
network artists.

Imports read tags with `lofty`, inspect audio properties, calculate content
identifiers, and upsert normalized library records. File paths remain
device-local.

## Playback

`player::Controller` owns the rodio audio thread. The application maintains
the logical queue and playback state, while the controller receives play,
pause, seek, volume, and prefetch commands. The next source is opened early
for gapless transitions. The analyzer publishes levels and scope samples for
Rhai visualization scripts.

OS media commands enter through `media.rs`; current metadata and position are
published back to the platform now-playing surface.

## Federation

Federation uses `music-dht` for discovery and byte streams:

- the local library publishes metadata-only item specifications;
- search merges DHT records, peer catalogs, and cached metadata;
- catalog requests provide richer artist/release views;
- audio requests stream content from peers;
- downloads may remain cached or be imported into the local library.

Federation is disabled until configured by the user. Paths are never
published as portable identifiers; content hashes and peer item IDs are used
instead.

## Trusted-device sync

`devices.rs` implements a separate trusted-device protocol over a dedicated
ALPN. Likes, playlists, membership changes, and playback control are
represented as an append-only operation log with materialized SQLite tables.
Hybrid logical timestamps and acknowledgements make offline merging and
tombstone compaction deterministic.

Pairing uses short-lived invites. Device-local file paths are deliberately
excluded from synchronized playback tracks; receiving devices resolve them
through content IDs, their own library, or federation metadata.

## Configuration and persistence

The `directories` crate selects platform-standard config, data, and cache
locations. Settings, keymaps, device identity, and federation configuration
are separate files. SQLite databases and downloaded covers/audio are stored
under application data/cache directories rather than the repository.

## Reliability rules

- Terminal raw mode, bracketed paste, and keyboard enhancements are restored
  on normal exit and panic.
- stderr from native audio libraries is captured into tracing so it cannot
  corrupt the alternate screen.
- Blocking work is kept out of the UI loop.
- Runtime failures are converted to visible status/events where recovery is
  possible.
- Device paths are not treated as portable network identities.
- Formatting, all-target compilation, Clippy, and unit tests should pass
  before a release tag is pushed.
