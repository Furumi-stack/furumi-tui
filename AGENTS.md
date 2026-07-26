# AGENTS.md

This file describes how to work safely in the Furumi repository. It applies to
the entire project.

## Project intent

Furumi is a cross-platform, federated P2P player for personal music libraries.
Every node must remain a complete local player without network access. Other
nodes add discovery, availability, trusted-device synchronization, and direct
track transfer; they are never a prerequisite for using the local library.

Read `ARCHITECTURE.md` before changing federation, device sync, playback
handoff, persistence, or runtime boundaries.

Preserve these architectural invariants:

1. Local import, browsing, playback, playlists, likes, and history work
   offline.
2. No central Furumi service becomes required for discovery, playback, or
   synchronization.
3. Federation membership does not imply trusted-device membership.
4. Remote state is merged, replicated, or cached locally; it is not treated as
   an always-available database.
5. Network, database, filesystem, image, and audio preparation work stays out
   of the interactive UI path.
6. Losing peers may reduce remote availability but must not invalidate local
   state.

## Toolchain and checks

The crate uses Rust edition 2024 and Rust 1.97 or newer.

Run the checks relevant to every code change:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets
```

`cargo clippy --all-targets` currently reports known warnings. Do not hide
them with broad `allow` attributes. Do not claim `-D warnings` passes unless
the existing warning set has actually been resolved.

Use `cargo fmt --all` after editing Rust. Keep `Cargo.lock` committed and
update it only when dependencies change.

## Source boundaries

- `src/main.rs` owns process startup, terminal restoration, Tokio setup, and
  platform media-loop integration.
- `src/app/state.rs` owns UI/navigation state.
- `src/app/action.rs` and `src/app/event.rs` define input intent and runtime
  results.
- `src/app/update.rs` performs state transitions and requests effects. It must
  not perform blocking I/O.
- `src/app/mod.rs` owns runtime orchestration and effect execution.
- `src/library/` owns SQLite library queries, models, imports, and migrations.
- `src/player/` owns rodio playback and audio analysis.
- `src/federation/` owns DHT discovery, peer catalogs, audio exchange, and
  federation caches.
- `src/devices.rs` owns trusted-device replication, membership, and playback
  coordination.
- `src/ui/` renders `AppState`; rendering code must not start background work
  or mutate persistence.
- `src/media.rs` owns OS media controls.
- `src/visualizer.rs` and `src/visualizations/` own the Rhai visualization
  host and bundled scripts.

Prefer adding a focused module when a responsibility has a clear boundary.
Do not split files solely to reduce line count if doing so introduces leaky
APIs or circular ownership.

Keep large test suites in their existing adjacent files:

- `src/app/update_tests.rs`
- `src/devices/tests.rs`
- `src/federation/tests.rs`
- `src/library/tests.rs`

## Application state and concurrency

`AppState` is the single source of truth for the TUI. The normal flow is:

```text
external event -> AppEvent -> Action/update -> Effect -> runtime -> AppEvent
```

Follow these rules:

- Keep update logic deterministic wherever practical.
- Return an `Effect` for work that requires runtime services.
- Report asynchronous results through `AppEvent`.
- Use `tokio::task::spawn_blocking` for blocking SQLite, hashing, tag parsing,
  or filesystem-heavy work invoked from async code.
- Do not hold a mutex guard across `.await`.
- Do not call terminal rendering APIs from background tasks.
- Preserve event sequence or request identifiers where they prevent stale
  search, download, or synchronization results from overwriting newer state.

## Library and SQLite

`library::Library` is the authority for local catalog persistence. Keep SQL in
the library/device persistence layers rather than spreading it through the UI
or runtime.

When changing schema or stored data:

- provide an upgrade path for existing databases;
- make migrations repeatable and safe on partially upgraded databases;
- preserve foreign-key and uniqueness invariants;
- distinguish durable user data from replaceable federation/cache data;
- test both the new behavior and migration-sensitive queries;
- never solve a schema problem by deleting or recreating a user's database.

Content identifiers are the bridge between independent libraries. Avoid
assuming that local numeric row IDs identify the same track on another node.

## Federation and wire protocols

Discovery and transfer are separate concerns:

- the DHT provides distributed discovery;
- peer catalog streams provide richer metadata;
- audio streams transfer content;
- the device-sync protocol replicates trusted personal state.

Treat protocol and serialized-data changes as compatibility changes:

- preserve existing ALPN values unless intentionally introducing a new
  protocol version;
- retain backward-compatible `serde` defaults for fields added to wire types;
- tolerate peers with older or partial metadata;
- bound incoming messages and validate untrusted lengths/identifiers;
- keep catalog/federation authority separate from trusted-device authority;
- assume peers can disappear between discovery and transfer;
- keep retries and fallback sources idempotent.

Do not introduce a required coordinator, registry, account server, or canonical
network database.

## Trusted-device synchronization

Device sync is offline-first. Operations may arrive late, more than once, or
in a different order.

When changing it:

- preserve operation-ID deduplication;
- use the existing hybrid logical time ordering rules consistently;
- ensure deletes survive offline replicas through tombstones;
- compact tombstones only after the required acknowledgements;
- keep snapshots able to repair peers that missed older operations;
- treat membership and revocation as replicated durable state;
- keep playback commands targeted and deduplicated;
- test merge behavior from at least two operation orders.

Do not replace eventual reconciliation with assumptions about a continuously
connected leader.

## Playback and remote resolution

The application owns the logical queue; `player::Controller` owns physical
audio playback.

Remote and local tracks should continue through the same queue and UI model.
When a track is unavailable locally, resolve it asynchronously and replace or
materialize the pending entry without blocking the event loop.

Preserve:

- queue position and prefetch indexes when inserting or removing tracks;
- shuffle/repeat behavior;
- pause and seek state during device handoff;
- fallback to another advertised source when a peer disappears;
- the distinction between cached audio and content imported into the durable
  library.

## UI and key bindings

The TUI is keyboard-first and cross-platform.

- Keep rendering pure over `&AppState`.
- Use semantic `Action` values rather than checking raw keys inside views.
- Add default bindings in `src/config/default_keymap.toml`.
- Preserve user overrides and context-specific bindings.
- Account for narrow terminals, Unicode display width, and empty/loading/error
  states.
- Restore raw mode, alternate screen, bracketed paste, and keyboard
  enhancements on exit and panic.

Do not print to stdout/stderr while the alternate-screen UI is active; use
`tracing` and visible application status instead.

## Cross-platform work

Furumi supports Linux, macOS, and Windows.

- Keep OS-specific code behind narrow `cfg` boundaries.
- Do not introduce shell-only behavior into portable paths.
- Use platform data/config/cache directories through the existing config
  helpers.
- Consider path encoding, separators, and non-UTF-8 filesystem values.
- Changes to media controls, terminal setup, clipboard behavior, or audio
  devices require explicit review of all three platforms.

If only one target can be exercised locally, state which platform-specific
paths remain unverified.

## Tests

Add tests at the closest stable boundary:

- pure state transitions in `app/update_tests.rs`;
- SQLite behavior and migrations in `library/tests.rs`;
- operation merge, tombstone, membership, and playback sync behavior in
  `devices/tests.rs`;
- search/ranking/federation conversion in `federation/tests.rs`;
- protocol-specific tests beside `federation/audio.rs` or
  `federation/catalog.rs` when appropriate.

Prefer in-memory SQLite databases and deterministic fixtures. Temporary files
must use unique names and must not depend on a developer's music library,
configuration, home directory, network peers, or audio hardware.

Do not remove a test merely because a refactor makes it inconvenient. Update
it to assert the preserved behavior.

## Documentation and releases

Keep public documentation aligned with the decentralized product model.
README content should explain user value and setup; `ARCHITECTURE.md` should
explain architectural decisions and invariants rather than restating source
code.

Release archives are produced by `.github/workflows/release.yml` and must
include the binary, `README.md`, and `LICENSE`.

The project is licensed under WTFPL version 2.
