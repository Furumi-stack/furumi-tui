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
- device membership and revocation;
- acknowledgements and synchronization progress;
- playback state, commands, and handoff information.

Pairing establishes the trust relationship. After that, changes are replicated
through an append-only operation log and applied to materialized local tables.

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

Jam never exchanges trusted membership, likes, playlists, or listening
history. Volume remains local. Commands carry unique IDs and are retried until
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

This is eventual consistency scoped to a trusted device group. A temporarily
offline laptop can modify playlists, another device can continue playback, and
both can later converge without a permanently available coordinator.

Membership changes use the same replicated model. Revocation is state that
must propagate and converge, not an ephemeral server-side session flag.

## Playback across devices

Playback has one logical state but remains physically local to the device
producing audio.

The synchronized state describes the queue, current item, position, pause
state, volume, shuffle/repeat mode, and the active playback owner. Commands are
targeted and deduplicated. Handoff transfers intent and position; the receiving
device resolves the track against its own library or federation sources before
starting its audio engine.

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
| Device operation log and replicas | SQLite | Offline synchronization |
| Versioned track embeddings | SQLite | Durable, locally rebuildable similarity data |
| Federation catalog cache | SQLite/cache | Faster network browsing |
| Audio and artwork cache | Filesystem cache | Reusable fetched data |
| Settings, keymap, identity | Platform config/data dirs | Node configuration |
| Queue and playback snapshots | Application/device sync state | Continuity and handoff |

Caches are replaceable. The local library and device operation log are durable.
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
