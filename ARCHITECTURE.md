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

## Persistence boundaries

Furumi stores different kinds of state according to their lifetime:

| State | Storage | Role |
| --- | --- | --- |
| Library, playlists, likes, history | SQLite | Durable local source of truth |
| Device operation log and replicas | SQLite | Offline synchronization |
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
