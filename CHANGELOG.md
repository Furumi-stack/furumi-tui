# Changelog

All notable changes to Furumi are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- "VFD panel" visualization: a vacuum-fluorescent hi-fi front panel with
  thin segment digits, a stereo peak level meter (bars or needle VU meters),
  a 25-band analyzer with three levels of glow, a nonlinear scale and peak
  hold, a peak frequency readout with hold, tone fans, a signal path
  schematic with a travelling pulse, the transport word in segment letters,
  mode lamps (REPEAT, SHUFFLE, source, LOUDNESS, MUTE, SEARCH, OVER), a tape
  counter and a dot-matrix title line (Latin and Cyrillic). Effects: a
  power-on segment test, a counter "search" on track changes, static on
  track and source changes, a source block that relights with its new
  value, colours that fade into unlit phosphor on pause, a night dimmer, a
  slow filament "breath", slightly uneven digit brightness and a standby
  clock when nothing is loaded. Display modes "full", "spectrum",
  "minimal" and "deck" switch with Shift+M or can be pinned with
  `DISPLAY_MODE()` at the top of the script, like the palettes ("classic",
  "green", "amber", "ice"); a mode that does not fit the window steps down.
  A switch looks like hardware: the time and L/R meters stay lit, the other
  sections go dark and light up one by one, and a row of mode lamps shows
  the choice for a moment. The analyzer's display range follows the
  loudness of the music (the measurements stay absolute), and deck mode has
  AMS, NR, AUTO REVERSE and REW/FF lamps with a counter that races during a
  seek. Bars settle with a little inertia, the analyzer's
  tint runs from warm amber in the bass to light yellow in the treble, peak
  marks fall with a little gravity after their hold, and the transport word
  uses 14-segment letters with faint unlit segments.
- `Shift+M` in the fullscreen visualizer switches its display mode; the
  choice is saved in `visualizations.toml`.
- Visualization protocol v3: a real third-octave spectrum (31 bands) with
  peak hold, left/right RMS and held peaks, the sample rate, and track,
  queue and transport details in the script input; `render` may return
  `#{ commands, state }` to keep state between frames (with `dt` and
  `frame`); new braille `dots` and `line` commands for thin drawing; bold,
  dim, italic, underline and reversed attributes on any command. v2 scripts
  keep working unchanged. Scripts also get the local time as
  `clock_hour`, `clock_minute`, `clock_second` and `clock_millis`, and the
  display mode as `mode`, a counter each script takes modulo its own number
  of modes.

### Fixed

- The visualization clock shows local time instead of UTC.

## [0.5.0] - 2026-11-20

### Added

- Sharing a podcast in the federation (`Shift+F` in the `podcasts` context,
  off by default): the synced share decision makes each device publish a
  signed public card of the podcast in the FRID discovery overlay
  (`podcast_dht`, advertised in the capability manifest); share-off and
  unsubscribe revoke it with a tombstone. Feed URLs that look private are
  published only after a confirmation on that device. The list and side
  panel show whether a podcast is shared and whether this device publishes
  it.
- Turning sharing off or unsubscribing revokes this device's card at once,
  and quitting waits up to 4 s for that (also for a change still being
  saved); the side panel says when a private podcast's card is not revoked
  yet (pending or failed) or may not be (federation off: a card of an
  earlier session cannot be ruled out). A privacy confirmation covers one
  share decision: turning sharing off (or on again after that) on any
  trusted device asks again, and until then this device holds the podcast
  back and revokes the card it published for the earlier decision. A
  confirmation whose decision could not be saved is dropped. Cards leave out
  artwork and web links with an access key in the query or a member-feed
  host (random-looking path parts of public CDNs are fine), and both for a
  feed whose URL looks private.
- A device with other devices in its group publishes its cards only after
  it synced with one of them since it started (or once none is left that
  syncs podcasts), so a device that was closed while sharing was turned off
  elsewhere does not send that card again; until then it only revokes, and
  the side panel says it waits for a sync.
- Search (`/`, with the federation running) also finds public podcast cards.
  They are listed after the music results under "Podcasts · public cards" with
  a `[podcast]` marker, the number of publishing devices, and "✓ subscribed"
  when this device is subscribed (also through a feed alias). The podcast
  search runs apart from the federated music search (bounded at 15 s), so
  music results never wait for it; local-only search is unchanged.
- Enter on a podcast hit opens its card: title, author, categories,
  description, episode count, newest episode, publishing devices, feed URL
  and unverified moved-feed claims. `e` reads the seasons and episodes from
  the feed without subscribing. «Подписаться» (Enter) subscribes privately
  with the card's origin feed URL, so the podcast id is the card's, and
  reads everything from the feed itself; «Уже подписан» opens the podcast
  in the Podcasts tab instead, also when this device follows the feed under
  the newer address it moved to (known once the feed was read), and
  subscribing to such an older address adds no second subscription.
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
  stale audio URL triggers one feed check for the episode's new address. A
  lost connection stops playback at the saved position (the episode is not
  completed and the queue does not move on). Without a file size from the
  server, a closed stream counts as the end within the completion window of
  the duration the audio file states, or past 80 % of a duration only the
  feed announced; a superseded or stalled stream
  is cancelled at the next start or stop; pause pressed while an episode
  connects is kept; seeks that do not move the audio are not saved.
- Podcast key bindings in the `podcasts` context: `o`, `/`, `Shift+R`,
  `Ctrl+R`, `Shift+S`, `f`, `m`, `Shift+U`, `b`.
- An optional top-level `version` in `keymap.toml`; the current format is
  `version = 2`, and a `podcasts` binding context.

### Changed

- `furumi-tui` 0.5.0.
- Use published `music-dht` 0.6.0 and `furumi-library` 0.3.0 from crates.io;
  remove the temporary FRID fork patches.
- Self-update and release download links use the canonical
  `Furumi-stack/furumi-tui` GitHub repository.
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
  they do not take playback over after two minutes of silence. When such a
  device does take over, the "Private item" entries it echoes back as music
  are left out of the queue here, with a status message.
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

- Isolate log privacy and identity-lock lifecycle tests from process-wide
  state and descriptor inheritance in parallel tests.
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
