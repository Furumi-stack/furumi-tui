# Changelog

All notable changes to Furumi are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - unreleased

### Added

- A Podcasts tab on `4`: subscribe to RSS/Atom feeds by URL, browse
  podcast → seasons → episodes with saved position, played state, duration and
  date, search episodes, filter by listening state, change the order, mark
  episodes played or unplayed, check one or all feeds, and unsubscribe (progress
  and history are kept). Feeds are checked in the background with conditional
  requests and back-off; permanent redirects become feed aliases.
- Podcast data uses the shared FRID store (`furumi_library::podcasts`) in
  `podcasts.sqlite3`; a database of the earlier podcast test build is migrated
  in place, and a newer or foreign file is refused without changes.
- Podcasts sync between trusted devices (device-sync protocol 4):
  subscriptions, unsubscribing, feed addresses, episode progress ("played",
  position, "start over") and listening sessions. A podcast subscribed on one
  device appears on the others without its URL; each device checks the feed
  itself. Podcast operations are applied to the podcast store, kept in the
  sync log and relayed; snapshots carry the podcast state (bounded) so new
  devices and devices that upgrade from an older version catch up. Periodic
  progress saves are coalesced in the log.
- A queue with podcast episodes is handed to other devices with the episode,
  its position and pause; the receiving device does not need the
  subscription.
- Podcast episodes play from the Podcasts tab and the shared queue (mixed
  with music): audio streams from the publisher through a bounded HTTP Range
  buffer, Play continues from the saved position, `b` starts over, and the
  position is saved every 20 s and on pause, seek, stop, track change and
  exit. Completion and listening sessions follow the shared FRID rules. A
  stale audio URL triggers one feed check for the episode's new address.
- Podcast key bindings in the `podcasts` context: `o`, `/`, `Shift+R`,
  `Ctrl+R`, `Shift+S`, `f`, `m`, `Shift+U`, `b`.
- An optional top-level `version` in `keymap.toml`; the current format is
  `version = 2`, and a `podcasts` binding context.

### Changed

- `music-dht` 0.6.0 and `furumi-library` 0.3.0.
- Queue items are keyed by their source: podcast episodes by episode id,
  library rows as `lib:<id>` instead of `local:<id>`. Likes, playlists,
  music history, similarity, share links and metadata edits skip episodes.
- Device sync speaks protocol 4 and uses the shared `music_dht::device_sync`
  wire types instead of private copies; v3 messages are unchanged byte for
  byte (golden tests). Devices below protocol 4 never receive podcast
  operations, podcast snapshot sections or a queue with episodes: handing
  such a queue to them is refused ("needs a Furumi update"), and their
  playback commands are ignored while the queue has episodes. They still get
  the playback coordination, with each episode shown as "Private item", so
  they do not take playback over after two minutes of silence.
- Jam guests see the host's podcast episodes as "Private item" (no title,
  feed or address). Guest changes that add, drop or duplicate them, or seek
  in them, are refused; a guest leaving a Jam drops the placeholders.
- Controlling another v4 device may queue and play episodes there; an
  episode started by another device's command at position 0 resumes from its
  saved position.
- Tab order is now `1` Library, `2` Playlists, `3` Queue, `4` Podcasts,
  `5` Settings, `6` Logs, for the tab bar, Tab/Shift+Tab, and the default
  number keys.
- Keymap files without `version` keep their tab shortcuts: `GoToTab` indices
  are remapped in memory (3 → 4 Settings, 4 → 5 Logs, and 5 → 3 Podcasts
  from the prototype podcast build) with a non-blocking startup notice. The
  file is not rewritten; `version = 2` files are used as written. A `version`
  key placed inside a `[[keymaps]]` entry is reported as an error instead of
  being ignored.
- The tab bar shortens titles, then shows only numbers (keeping the active
  tab's full title) on terminals too narrow for all six tabs.
- A keymap notice no longer hides a "logging disabled" startup warning; both
  are shown.
- The help window names the target tab for number-key shortcuts
  ("Go to Settings") instead of "Go to tab N".

### Fixed

- The protocol version stored for another device came from this build's own
  constant (on profile announcements and accepted pairings), not from the
  device. Versions now come from the device's own reports; relayed profiles
  only raise them. Invites and profile announcements carry the version, and
  the first start of a new version announces it.

## [0.2.8] - 2026-09-02

### Added

- Optional offline music-similarity search for local tracks, backed by
  versioned SQLite embeddings and a replaceable exact in-memory cosine index.
- Automatic SHA-256-verified download and local inference for the first ONNX
  embedding model, with retained model/profile generations and background
  backfilling of existing library tracks.
- Similarity settings for enablement, model/profile information, numeric worker
  count, derived-data cleanup, processing progress, and federation privacy
  consent.
- Track-seeded similarity search from the track-information popup, including
  bounded federated queries to compatible known peers.
- The `furumi-fd/similarity/1` protocol in the visible protocol-version status.
- Decentralized `similarity_dht` routing with signed anonymous two-level LSH
  summaries, multi-probe lookup beyond the locally known peer set, and known-
  peer fallback during gradual network upgrades.

### Changed

- Similarity wire types, bounds, validation, and stream framing now come from
  the shared `music-dht 0.4` API so native, web, and future clients can
  interoperate without sharing an embedding implementation.
- Existing SQLite embeddings are backfilled once with compact 256-bit routing
  signatures; new embeddings store them immediately without changing exact
  local cosine search.
- A similarity result page keeps the source track first as query context while
  excluding it from the actual nearest-neighbor ranking, labels the mode as
  `Search similar to`, and suppresses near-identical embeddings across releases
  and federated peer responses.
- Preprocessing profiles now open a read-only details window describing their
  audio selection, resampling, spectrogram, patching, and aggregation contract.

### Fixed

- Federation now recovers automatically after sleep, prolonged idle, or a
  degraded rendezvous transport while preserving local playback and state.
- Current-track information (`Shift+I`) now uses the enriched queue entry, so
  it shows the same complete metadata and similarity action as `I` on that
  track in the queue.

## [0.2.5] - 2026-08-02

### Added

- Command history navigation with the Up and Down arrow keys.
- A copy-friendly plain terminal view for connection tickets, device and Jam
  invites, and track-sharing links.
- A configurable permanent music directory with write validation and an
  optional safe migration of existing Furumi-managed files.
- Queue reordering for one track or a `Shift+V` selection with `Alt+K` and
  `Alt+J`.
- Reproducible Nix/devenv tooling with Rust 1.97 and ALSA development files.

### Changed

- Sequential `a` actions now build one ordered play-next block instead of
  reversing independently inserted tracks.
- Federated artist images and release covers are stored beside permanent music
  in an `Artist/Release` directory tree.

### Fixed

- Running a development build next to another Furumi instance no longer lets
  an MPRIS name collision disable terminal raw mode and freeze keyboard input.
- `nix develop` now keeps devenv state outside the read-only Nix store and
  isolates Rust 1.97 build artifacts from other toolchains.
- Music-directory validation and migration now reject overlapping changes,
  resolve canonical paths, and produce Windows-portable managed filenames.

[Unreleased]: https://gt.hexor.cy/ab/furumi_tui/compare/v0.2.8...HEAD
[0.2.8]: https://gt.hexor.cy/ab/furumi_tui/compare/v0.2.7...v0.2.8
[0.2.5]: https://gt.hexor.cy/ab/furumi_tui/compare/v0.2.4...v0.2.5
