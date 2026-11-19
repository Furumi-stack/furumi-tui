# Furumi architecture

Furumi is an autonomous music player that can cooperate with other Furumi
players. The architecture starts from one constraint: **a node must remain a
complete and useful player when every other node is unavailable**.

Networking therefore extends a local player instead of becoming a prerequisite
for it. There is no control plane, account service, canonical catalog, or
server-owned source of truth.

## Architectural goals

The design optimizes for five properties:

1. **Local autonomy** — importing, browsing, playback, playlists, likes, and
   history work entirely on one device.
2. **No central failure domain** — discovery, catalog exchange, streaming, and
   synchronization do not depend on a Furumi-operated service.
3. **Offline tolerance** — trusted devices may change state independently and
   reconcile after reconnecting.
4. **Incremental federation** — one node is complete; every additional node
   increases availability and the amount of discoverable music.
5. **Explicit trust boundaries** — personal-device replication and wider
   music federation are different protocols with different authority.

These goals are more important than maintaining a globally identical view of
the network. Furumi prefers useful local progress and eventual reconciliation
over distributed consensus.

## The node model

Every running Furumi instance contains the same four capabilities:

```text
┌─────────────────────────────────────────────────────────┐
│                      Furumi node                        │
│                                                         │
│  Local library ── Playback engine ── TUI / media keys   │
│        │                 │                              │
│        ├── Trusted-device replication                  │
│        │                                               │
│        └── Federation: discovery, catalog, audio        │
└─────────────────────────────────────────────────────────┘
```

The local SQLite library is authoritative for that node. Network data is
merged into local views or materialized as pending remote content; it does not
replace the local database with a remote database abstraction.

This keeps the core behavior predictable:

- disconnecting never makes the local collection unavailable;
- downloaded content can become ordinary local content;
- a peer disappearing reduces availability but does not invalidate local
  state;
- nodes may join and leave without electing a leader.

## Two network layers

Furumi deliberately separates **trusted-device synchronization** from
**federated music exchange**.

### Trusted-device synchronization

This layer connects devices owned or trusted by the same user. It carries
personal state such as:

- likes and playlist operations;
- podcast subscriptions, episode progress and listening sessions;
- device membership and revocation;
- acknowledgements and synchronization progress;
- playback state, commands, and handoff information.

Pairing establishes the trust relationship. After that, changes are replicated
through an append-only operation log and applied to materialized local tables.
The wire types are the shared `music_dht::device_sync` definitions (protocol
v4); golden tests keep the v3 JSON of furumi-tui 0.3.x byte for byte.

Each device stores the protocol version that the other devices report
themselves (their hello and pairing profiles, the sender's own entry of a
`devices` list, their own profile announcements). A relayed profile only
raises a stored version, because older relays list every device with their
own constant; this build never stores its own version for another device.
Everything a peer cannot parse is gated by that version: podcast operations,
podcast snapshot sections and playback state that contains podcast items
reach only v4 peers (`op_allowed_for_peer`, `SyncSnapshot::for_peer`,
`playback_allowed_for_peer`). An older client would reject a whole message
over one unknown operation kind, or silently drop an episode it does not
understand.

### Music federation

Federation connects independent libraries. It provides:

- decentralized discovery through the DHT;
- automatic publication of searchable local catalog metadata;
- richer artist and release catalogs fetched from peers;
- direct audio transfer when a selected track is not available locally.

Federation does not grant another peer authority over personal playlists,
likes, or device membership. A node may participate in federation without
joining another user's trusted-device group.

Keeping these layers separate prevents discovery convenience from silently
becoming a synchronization trust decision.

### Federation Jam control

Jam is a third, deliberately narrow authority boundary. A host creates an
opaque `frid://j/...` runtime capability and remains the only node producing
audio. Other TUI peers use a dedicated Jam ALPN to submit the same portable
playback commands used by connected-device control and receive the host's
playback snapshot. They receive queue metadata, not audio.

Jam never exchanges trusted membership, likes, playlists, listening history
or podcast state. The host's podcast episodes are private: the published
snapshot replaces them with opaque placeholders (`redact_for_jam`), guest
commands that add episodes are dropped, and a guest's placeholders are mapped
back to the host's episodes (`restore_jam_private_items`); a change that
drops, duplicates or seeks in them is refused. Volume remains local. Commands carry unique IDs and are retried until
the host acknowledges them, while inactive participants expire from the
runtime session. Regenerating the capability or restarting the host invalidates
the previous link.

## Discovery and direct communication

Furumi separates finding content from transferring it.

```text
                 discovery plane
 Local catalog ───────> DHT <─────── Other catalogs
                            │
                            │ peer + content identity
                            v
                    direct P2P connection
                     ├── catalog protocol
                     ├── audio protocol
                     └── device-sync protocol
```

The DHT is the distributed index. Nodes publish compact searchable
descriptions of their local library and query the network without contacting a
central search service.

Opening a local artist or release card in a network-enabled library view
enriches it asynchronously. DHT matches identify likely catalog owners, while
a bounded set of known peers can also answer directly when the index is
incomplete. Card results are replaceable snapshots: reopening a card or
periodic maintenance retries discovery, with at most one request per card in
flight. A failed refresh keeps the last usable snapshot, and never prevents
browsing the local tracks.

Similarity discovery uses a separate schema-independent DHT overlay. A node
derives a deterministic 256-bit routing signature from every local embedding,
groups them into fixed two-level LSH buckets, and publishes compact summaries
containing only fine-bucket representatives, its peer identity, and the ticket
needed to dial a previously unknown owner. The owner signs every summary with
its existing transport key, so storage peers can relay and cache it but cannot
impersonate or modify it. Summaries expire with the ordinary library-record TTL
and are replaceable federation cache, never local library authority.

A similarity search first performs bounded multi-probe LSH lookups to rank
likely owners, then sends the existing normalized-vector request directly to
at most 16 peers initially and 48 on fallback. Known peers remain a rollout
fallback. The model, preprocessing, durable embeddings, exact cosine search,
and consent policy remain client-owned; `music-dht` owns only compatible
routing math, signed records, replication, and wire bounds.

Once a peer is known, communication moves to direct P2P streams provided by
iroh through `music-dht`. Furumi defines separate application protocols for
catalog requests, audio transfer, and trusted-device synchronization. This
keeps discovery traffic small and lets large or private exchanges happen only
between the participating peers.

Relay-assisted connectivity may help peers establish a route, but relays do
not become catalog authorities or application-state owners.

## Identity and content resolution

Network operations refer to content independently of any one library row.
Content identifiers allow a node to ask:

1. Is this track already present in my local library?
2. Does one of my trusted devices have it?
3. Which federation peers currently advertise it?

Resolution follows that order conceptually: prefer a ready local source, reuse
known content where possible, and fetch from a peer only when necessary.

A federated track can initially exist as a lightweight pending item in a queue
or playlist. When playback reaches it, Furumi resolves an available peer,
starts the transfer, and either uses the cache or imports the result into the
local library. The rest of the player continues to work with the same
`TrackItem` model, so local and remote availability do not require separate
playback systems.

## Offline-first synchronization

Trusted devices do not share a live database connection. Each device records
operations locally and exchanges them when connectivity returns.

The synchronization model combines:

- immutable operation identifiers for deduplication;
- hybrid logical timestamps for deterministic last-writer decisions;
- materialized tables for fast UI queries;
- tombstones so deletion survives offline replicas;
- per-peer acknowledgements to determine when old tombstones can be compacted;
- snapshots to repair a peer that missed older operations.

Vectors only grow, so operations hidden from a peer while it ran an older
protocol are never sent again after it upgrades. Snapshots are the recovery
path for that case; the podcast sections carry the subscription, share and
progress registers and the most recent listening sessions, bounded by the
shared FRID limits and by the line budget of each message.

This is eventual consistency scoped to a trusted device group. A temporarily
offline laptop can modify playlists, another device can continue playback, and
both can later converge without a permanently available coordinator.

Membership changes use the same replicated model. Revocation is state that
must propagate and converge, not an ephemeral server-side session flag.

## Playback across devices

A persisted device identity has exactly one live coordinator. The TUI holds
an OS file lock beside its device-sync database for the entire runtime; two
installations using the same user data directory cannot open it concurrently.
Network polling is independent per trusted peer, with one in-flight exchange
per peer and a bounded deadline. A stalled peer must not delay the next poll
of a healthy output. Poll tasks are cancelled when federation shuts down.

Playback has one logical state but remains physically local to the device
producing audio.

The synchronized state describes the queue, current item, position, pause
state, volume, shuffle/repeat mode, and the active playback owner. Commands are
targeted and deduplicated. Handoff transfers intent and position; the receiving
device resolves the track against its own library or federation sources before
starting its audio engine. Each queue item names its source: a podcast episode
travels as `PlaybackSource::PodcastEpisode` with the reference needed to play
it (the receiving device streams it from the publisher at the handed-off
position, with or without a subscription). A queue with episodes is not handed
to a device below protocol v4, and playback commands from such a device are
ignored while the local queue has episodes. Such a device still gets the
playback snapshot with its coordination envelope, but with every episode
replaced by an opaque "Private item" (`redact_for_jam`): an engine renews the
owner's liveness only from the owner's own announcements, so an old device
that heard nothing would take over by automatic failover after
`owner_timeout_ms` and replace the queue. This departs from FRID's
`PLAYBACK_PROTOCOL.md` rule 1 ("otherwise send `None`") and is reported to
FRID as a contract gap.

Active-device leases and idle timing prevent stale snapshots from immediately
taking control after a device reconnects. This provides practical coordination
without introducing a central playback arbiter.

## Local application architecture

Inside one node, Furumi uses an event-driven state machine:

```text
terminal / player / database / network / OS media events
                           │
                           v
                       AppEvent
                           │
                    input → Action
                           │
                           v
                    update(AppState)
                           │
                     optional Effect
                           │
                           v
                  asynchronous runtime work
                           │
                           └──────────> AppEvent
```

`AppState` is the single source of truth for the interface. The update layer
performs deterministic state transitions and requests effects; it does not
perform blocking I/O. SQLite access, imports, artwork decoding, DHT queries,
peer transfers, and audio preparation run through runtime services and report
their results as events.

This structure gives the TUI three important properties:

- rendering is a pure projection of current state;
- input behavior can be tested without starting audio or networking;
- slow peers and large imports cannot block terminal interaction.

The audio engine is similarly isolated. The application owns the logical
queue, while `player::Controller` owns rodio playback and receives explicit
commands. Prefetching prepares the next source before the current item ends.

### Versioned key bindings

Key bindings are semantic `Action` values loaded from the bundled defaults and
merged with the user's `keymap.toml`. Some actions carry positional meaning
(`GoToTab(index)` follows the displayed tab order), so changing the tab order
changes what an existing user file means. The keymap file therefore carries a
format `version`. Files from an older format are adapted in memory when they
are loaded and the user is told once per start; Furumi never rewrites a
user's configuration. The current format, version 2, orders tabs as Library,
Playlists, Queue, Podcasts, Settings, Logs; unversioned files use the earlier
order and have their tab indices remapped. A `version` key that TOML attached
to a binding (written after a `[[keymaps]]` header) is rejected, so a versioned
file is never silently mistaken for a legacy one.

## Podcasts

Podcasts are a separate content type, not library tracks. The shared contract
lives in FRID: `music_dht::podcast` defines podcast and episode identity, feed
URL canonicalization, the RSS/Atom parser and the merge rules, and
`furumi_library::podcasts::PodcastStore` persists the catalog, the personal
registers (subscription, share, progress) and listening sessions in
`podcasts.sqlite3`. The TUI does not re-derive ids or merge rules.

The TUI owns what the contract leaves to clients:

- **Fetching.** Feeds are fetched with conditional requests (ETag,
  Last-Modified), bounded size (32 MiB per feed, 10 MiB per cover) and time
  (15 s to connect, 60 s per request, 120 s for a whole fetch with its
  redirects), and at most 10 redirects (http and https only) followed by
  hand. The
  address that the leading permanent hops (301/308) lead to becomes a feed
  alias and the fetch URL, even when a temporary hop (CDN, tracker) follows
  it; a chain that starts with a temporary hop never moves the feed.
  Validators are dropped after a permanent redirect because they belong to
  the old address. The prototype database's validators are dropped once at
  import, so the first check parses the feed (seasons and episode numbers
  were not stored by the prototype).
- **Scheduling.** The store keeps each feed's local fetch state (validators,
  last error, consecutive failures, next check). A timer asks it which
  subscriptions are due; feeds are checked at most a few at a time, a success
  waits an hour, and failures back off exponentially (honouring
  `Retry-After`). Errors never clear the stored catalog.
- **Runtime boundary.** Every store call, fetch and parse of the app runs on
  the blocking pool or as a task and reports back through `AppEvent`s;
  `update::podcasts` holds the pure state transitions. The device-sync
  stream handlers call the store (snapshot export and merges) inside
  `block_in_place`, so a store busy with a large feed refresh does not hold
  a Tokio worker's other tasks; they are never called on the UI thread. Store open failures (newer schema,
  foreign file) disable only the Podcasts tab and leave the file untouched.
- **Sync log.** Local actions (subscribe, unsubscribe, mark played, progress)
  draw their operation stamps from the device-sync log through
  `DeviceSync::run_podcast_action`, and the operations are appended with
  exactly those stamps, so the store's registers and the log agree. The lock
  order is podcast store, then sync log; device sync never calls the store
  while it holds the log. Only the newest progress operation of an origin per
  episode stays in the log (later operations of one origin always win the
  register comparison).
- **Device sync.** Received podcast operations are kept in the log (and
  relayed) and merged into the store with `PodcastStore::apply_remote_op`;
  invalid ones are skipped but acknowledged. Operations that arrive before
  the store opens wait in `sync_podcast_pending` and are applied when it is
  attached. Snapshot sections are exported with `fill_snapshot` and merged
  with `apply_snapshot` after the operations of the same message. The UI
  reloads what it shows after such changes and checks the feed of a
  subscription that came from another device.
- **Queue identity.** An episode is a `TrackItem` with an explicit
  `podcast` reference (`TrackItem::podcast_episode`); its numeric id is a
  shared sentinel and never identifies it. Queue keys check the episode id
  first (`ep:…`), then music content ids, federated sources and library rows.
  Music-only paths (likes, playlists, history, content-id resolution,
  federation download, similarity, share links, metadata edits, release and
  artist jumps) check `is_podcast()` and skip episodes. On the wire an
  episode is `PlaybackSource::PodcastEpisode`; received ones become episodes
  again (`TrackItem::from_playback_track`) and the library source mode never
  drops them. Jam private placeholders are numbered queue entries that go
  back on the wire as `PlaybackSource::Private`.
- **Playback.** Starting an episode resolves its current audio URL and
  resume position from the store, opens `podcasts::stream` (a bounded
  Range-request reader on its own thread), decodes and positions it with
  coarse seeking, all off the UI and audio threads, then hands the source to
  the audio thread with the player's source generation. Any later start or
  stop bumps that generation, so a slow stream never replaces what the user
  chose since. A stale audio URL (4xx) triggers one feed refresh and a retry
  with the address the feed lists for the same episode id. A stream that
  cannot seek does not start at a saved position it cannot reach, and seeks
  in it (or in an episode still connecting) are refused. The stream is
  cancelled by the same generation: a superseded start stops connecting or
  reading up to its resume offset, and a read blocked on a stalled server
  inside the audio callback returns within 100 ms, so it cannot hold the
  audio thread past the next stop or start. The player does not report the
  end of a superseded source. Pause and resume pressed while a source is
  prepared update the intent it starts with.
- **End of a stream.** The reader records whether the decoder stopped on a
  read error (lost connection, expired URL, cancellation) and whether the
  server announced the file size. Only an end that the server described, or
  one within the completion window, completes the episode and moves the
  queue on; otherwise the last seen position is saved and playback stops on
  the episode (`playback::played_to_the_end`).
- **Progress.** `podcasts::progress` runs one writer thread so every
  progress write of this device is ordered. The app tick follows the
  position and actual listening time and sends: an explicit start (or
  "start over"), checkpoints every 20 s and on pause, explicit seeks (saved
  once the audio thread reports that the seek worked,
  `PlayerEvent::Seeked`), completion (`reaches_completion` or the real end
  of the stream) and an end with the last
  seen position on stop, track change, handoff and exit. When the store
  reports a newer generation (another device started over, for example), the
  playback stops checkpointing until the user's next explicit action.
  Listening sessions are recorded at pause, completion and end; the store
  keeps those that pass the shared threshold. Queue items carry the latest
  progress generation this device saw, so a handoff target starts its
  session above it.

## Scripted visualizations

Visualizations are an extension boundary rather than hard-coded rendering
paths. Rust owns audio sampling, script execution, validation, and terminal
drawing; Rhai scripts own the visual composition.

```text
rodio source
     │
     v
audio analyzer ──> normalized features + scope samples
                                      │
                                      v
                              Rhai render(input)
                                      │
                                      v
                            validated draw commands
                                      │
                                      v
                              ratatui frame buffer
```

The player analyzer derives a bounded, renderer-independent input model:
energy, bass, mid, treble, beat strength, waveform samples, playback progress,
volume, pause state, track metadata, time, and terminal dimensions. Each
script implements `render(input)` and returns declarative commands such as
clear, cell, line, rectangle, trace, and text. Scripts never receive the
ratatui frame or audio engine directly.

This command boundary is intentional:

- scripts remain independent of Rust UI internals;
- the host validates command shapes, colors, coordinates, and arrays;
- drawing is clipped to the current terminal area;
- script failures become an in-UI visualizer error instead of corrupting the
  terminal or stopping playback.

Rhai files live in the user's visualization directory. The runtime discovers
them dynamically, compiles the selected script, caches its AST, and recompiles
it when the file modification time changes. A visualization can therefore be
created or edited while Furumi is running without rebuilding or restarting the
application. Bundled scripts use the same path and contract as user scripts,
so built-in and custom visualizations exercise the same runtime.

The v2 primitives are sufficient for small perspective wireframes without a
native 3D engine. The bundled morphing cube transforms eight vertices in Rhai
and batches its twelve edges into trace/vertical-line commands. Perspective,
audio-driven deformation, and depth cues stay in the user-editable script;
Rust rasterizes the resulting 2D commands. This deliberately avoids per-cell
script command maps and does not provide filled meshes or a depth buffer.
The richer cube preset adds bounded particle, ring, inner-cage, and echo
layers. Echo poses are reconstructed from earlier times with current audio
features, rather than retaining previous frames; particles have deterministic
lifetimes. Small terminals omit expensive layers and reduce particle counts.

The Rhai engine is configured as a sandboxed computation environment. Module
loading through `import` and `export` is disabled, no filesystem or network API
is exposed to scripts, and execution is bounded by limits on operations, call
depth, variables, functions, expression depth, and collection/string sizes.
Only the input map, Rhai language primitives, and a small set of mathematical
helpers are available.

The sandbox protects responsiveness and keeps visualization code in its
intended role: transforming current audio features into drawing commands. It
is not a plugin mechanism for accessing the library, network, or player
controls.

Fullscreen execution currently happens synchronously during UI drawing, with a fresh
Rhai scope for each frame; scripts should derive motion from time and the
already-smoothed audio features rather than rely on retained mutable state.
The 50 ms animation timer gives a nominal 20 FPS cadence (other events can
also trigger redraws). Operation limits are not wall-clock deadlines. The
host also checks the script modification time during drawing, so slow script
storage or expensive custom scripts can still delay input. Benchmark script
evaluation and buffer drawing separately from real terminal throughput before
increasing scene complexity or the refresh rate.

The embedded status surface uses the same Rhai engine, input map, command
decoder, and clipped buffer rasterizer with a one-row viewport. It has an
independent AST cache and a fixed user-editable script in
`visualizations/status/braille.rhai`, installed if absent. Exact copies of the
previous bundled default can be atomically upgraded; edited files are preserved. It is not a
fullscreen preset. During local playback the visual timer samples the audio
analyzer without increasing trusted-device publication frequency. At most
one blocking render job is in flight; excess ticks are dropped. File access,
compilation, evaluation, and buffer preparation happen on that worker.
An `AppEvent` delivers a prepared row to `AppState`; UI drawing only copies
cells. Its audio input comes from the decoded-source analyzer before output
volume scaling; the volume field remains informational. Pause suspends jobs
and retains the last frame, even when the row needs resizing. Generation
checks discard results invalidated by track changes, resize, pause/resume,
or stop. Script errors are confined to the row and retried so
editing the file repairs the display. Status scripts have a smaller operation
budget, bounded source/output sizes, and cannot draw outside their viewport.

## Music-similarity indexing

Similarity search is an optional local capability and is disabled by default.
When enabled, a background pipeline downloads a selected ONNX model, verifies
its pinned SHA-256 digest, decodes durable local tracks, and stores normalized
embeddings in the library SQLite database. Embeddings are keyed by an exact
fingerprint of the model artifact and preprocessing profile. Old profile rows
remain available while a new profile is calculated, and the in-memory exact
cosine index switches only after the replacement profile is usable.

The SQLite rows are the canonical derived store. The in-memory index can be
discarded and rebuilt, and neither is required for import, browsing, or
playback. Remote/cache-only tracks are never scheduled for local embedding.

Federated similarity uses a separate versioned direct-stream protocol. With
explicit privacy consent, the requester sends only a normalized embedding and
its profile fingerprint to a bounded set of known peers. It does not publish
queries to the DHT. Each peer searches its own active local index and returns a
bounded metadata result with a compact embedding SimHash. The requester uses
that signature to suppress near-duplicate recordings across peers without
receiving every result vector. Fan-out, concurrency, message sizes, and
timeouts are bounded; incompatible profiles are rejected. This direct
peer-selection layer can later be replaced by DHT routing without changing
local storage or ranking.

## Persistence boundaries

Furumi stores different kinds of state according to their lifetime:

| State | Storage | Role |
| --- | --- | --- |
| Library, playlists, likes, history | SQLite | Durable local source of truth |
| Podcast subscriptions, episodes, progress | SQLite (`podcasts.sqlite3`, FRID store) | Durable personal state (synced with trusted devices) and a refreshable episode catalog |
| Device operation log and replicas | SQLite | Offline synchronization |
| Versioned track embeddings | SQLite | Durable, locally rebuildable similarity data |
| Federation catalog cache | SQLite/cache | Faster network browsing |
| Audio and artwork cache | Filesystem cache | Reusable fetched data |
| Settings, keymap, identity | Platform config/data dirs | Node configuration |
| Queue and playback snapshots | Application/device sync state | Continuity and handoff |

Caches are replaceable. The local library, the podcast store's personal state
and the device operation log are durable.
This distinction lets maintenance and recovery code discard derived network
data without risking the user's collection.

## Failure model

Expected failures are treated as ordinary state:

- a DHT query may return partial results;
- an advertised peer may be offline by the time a track is requested;
- a transfer may stop and be retried through another source;
- trusted devices may reconnect with overlapping changes;
- cached metadata may be stale;
- an audio device may disappear during playback.

The architecture avoids converting these cases into global failure. Search can
show partial data, synchronization can resume, and playback resolution can try
another source. Errors return to the application as events so the UI can expose
them without terminating the node.

## Main implementation boundaries

The source tree follows the architectural responsibilities:

- `library/` owns the local catalog and import pipeline;
- `player/` owns audio playback and analysis;
- `similarity.rs` owns model acquisition, preprocessing, background indexing,
  and the replaceable exact in-memory index;
- `federation/` owns DHT-facing search, peer catalogs, and audio exchange;
- `podcasts/` owns feed HTTP (conditional requests, redirects, limits),
  refresh scheduling policy, the episode stream reader and the progress
  writer; the podcast model, identity, merge rules and SQLite store come
  from FRID;
- `devices.rs` owns trusted-device replication and playback coordination;
- `app/` owns state transitions and runtime orchestration;
- `ui/` renders the TUI;
- `media.rs` integrates platform media controls;
- `visualizer.rs` hosts programmable Rhai visualizations.

Dependencies should continue to point inward toward typed models and explicit
events. UI code should not own network tasks, network protocols should not
mutate UI state directly, and the local library should not depend on the
presence of federation.

## Architectural invariants

Future changes should preserve these rules:

1. A node must start and play its local library without network access.
2. No central Furumi service may become required for discovery, playback, or
   trusted-device synchronization.
3. Federation membership must not imply personal-device trust.
4. Remote state must be merged or cached locally, never treated as an always
   available database.
5. Network and storage work must remain outside the interactive UI path.
6. More peers should improve availability; losing peers should only reduce
   remote capabilities.
