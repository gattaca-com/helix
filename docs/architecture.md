# Helix Relay Architecture

The relay is a network of long-lived tasks connected by channels and shared state.
Each task is an independent execution context — a thread, a Flux tile, or a tokio
task — that runs for the lifetime of the process. The system's behavior emerges from
how these tasks communicate.

This document enumerates every long-lived task, what it does, and how it connects
to the rest of the system.

## Notation

```
A --[channel_type]--> B     A sends to B via channel
A --{shared_state}--> B     A writes state that B reads
A ===[network]==> B         A connects to B over the network
```

## Channels

These are the channels that connect tasks. Each is created once at startup.

| Name | Type | Capacity | From | To |
|------|------|----------|------|----|
| `head_event` | `broadcast` | 100 | Beacon SSE tasks | Housekeeper, ChainEventUpdater |
| `payload_attribute` | `broadcast` | 300 | Beacon SSE tasks | ChainEventUpdater |
| `event` | `crossbeam bounded` | 10,000 | SubWorkers, ChainEventUpdater, Housekeeper | Auctioneer |
| `sub_worker` | `crossbeam bounded` | 10,000 | API (builder submit) | SubWorkers |
| `reg_worker` | `crossbeam bounded` | 100,000 | API (registrations) | RegWorkers |
| `top_bid` | `tokio broadcast` | 100 | Auctioneer (via BidSorter) | Top Bid WS connections |
| `gossip` | `tokio mpsc` | 10,000 | gRPC Gossip Server | Gossip Processor |
| `network_broadcast` | `tokio broadcast` | 100 | RelayNetworkManager | Peer WS handlers |
| `api_events` | `tokio mpsc` | 100 | RelayNetworkManager API | Network Event Handler |
| `block_submissions` | `tokio mpsc` | 10,000 | Auctioneer (DB writes) | Block Submission Flusher |

## Tasks

### Beacon Layer

These tasks connect the relay to the Ethereum consensus layer.

#### 1. Beacon SSE Head Events (x N beacon nodes)

**Spawned by**: `subscribe_to_head_events` in `MultiBeaconClient`
**Runs on**: tokio
**File**: `beacon/beacon_client.rs`

Maintains a persistent SSE connection to a beacon node's `/eth/v1/events?topics=head`
endpoint. On disconnect, reconnects in a loop. Each head event (slot number + block
hash) is sent to the `head_event` broadcast channel.

```
Beacon Node ===[SSE]==> this --[head_event broadcast]--> Housekeeper
                                                     --> ChainEventUpdater
```

#### 2. Beacon SSE Payload Attributes (x N beacon nodes)

**Spawned by**: `subscribe_to_payload_attributes_events` in `MultiBeaconClient`
**Runs on**: tokio
**File**: `beacon/beacon_client.rs`

Same pattern as head events but subscribes to `payload_attributes` topic. Payload
attributes contain the proposer's fee recipient, timestamp, and other data needed
to build valid blocks.

```
Beacon Node ===[SSE]==> this --[payload_attribute broadcast]--> ChainEventUpdater
```

#### 3. Beacon Sync Monitor

**Spawned by**: `start_beacon_client`
**Runs on**: tokio
**File**: `beacon/multi_beacon_client.rs`

Polls all beacon nodes for sync status every 1 second. Selects the best (highest
head slot) and updates `best_index` so other code can prefer the most up-to-date
beacon node. Also records per-beacon sync metrics.

```
Beacon Nodes ===[HTTP]==> this --{best_index AtomicUsize}--> any code calling beacon
```

### State Management

These tasks maintain the relay's view of the chain and validator set.

#### 4. ChainEventUpdater

**Spawned by**: `start_housekeeper`
**Runs on**: tokio
**File**: `housekeeper/chain_event_updater.rs`

Central state distribution point. Receives head events and payload attributes from
beacon SSE. On each new slot, updates `CurrentSlotInfo` (read by API handlers for
timing decisions) and sends `SlotUpdate`/`PayloadAttributesUpdate` events to the
Auctioneer via the `event` channel.

Has a 60-second watchdog — if no events arrive, it logs an error. Outer loop
restarts the inner loop on panic.

```
--[head_event broadcast]-->     this --[event crossbeam]--> Auctioneer
--[payload_attribute broadcast]-->    --{CurrentSlotInfo}--> API handlers
                                      --{LocalCache}------> API handlers, Workers
```

#### 5. Housekeeper

**Spawned by**: `start_housekeeper` (submission instances only)
**Runs on**: tokio
**File**: `housekeeper/housekeeper.rs`

Runs heavy, per-slot maintenance that the Auctioneer shouldn't block on. On each
head event (or timeout at slot+4s):

- Updates proposer duties from beacon (every epoch)
- Refreshes known validators from DB
- Updates trusted proposers
- Reloads builder infos
- Triggers inclusion list fetch (if configured)
- Saves merged blocks from previous slot

Each update is spawned as a separate tokio task to avoid blocking the main loop.

```
--[head_event broadcast]--> this --[event crossbeam]--> Auctioneer
                                 --{LocalCache}-------> API handlers, Workers
                                 --[DB writes]--------> PostgreSQL
```

### Auction Core

The hot path. These run on dedicated CPU-pinned threads via the Flux tile framework.

#### 6. SubWorker (x N cores)

**Spawned by**: `spine.start` via `attach_tile`
**Runs on**: CPU-pinned Flux tile thread
**File**: `auctioneer/worker.rs`

Receives raw block submissions and get_payload requests from the API layer via
the `sub_worker` crossbeam channel. Performs CPU-intensive work: decompression,
deserialization (SSZ/JSON), signature verification, and initial validation
(correct slot, known builder, value checks). Validated submissions are forwarded
to the Auctioneer via the `event` channel.

Uses `rx.try_iter()` to drain the queue each tick — processes all pending work
before yielding.

```
--[sub_worker crossbeam]--> this --[event crossbeam]--> Auctioneer
                                 --{LocalCache}-------> (reads builder info, slot data)
```

#### 7. RegWorker (x N cores)

**Spawned by**: `spine.start` via `attach_tile`
**Runs on**: CPU-pinned Flux tile thread
**File**: `auctioneer/worker.rs`

Receives batches of validator registrations from the API layer. Verifies BLS
signatures (the expensive part) and writes valid registrations to the DB cache.
Uses `rx.recv_timeout(50ms)` to batch work.

```
--[reg_worker crossbeam]--> this --{DB validator_registration_cache}--> PostgreSQL
```

#### 8. Auctioneer

**Spawned by**: `spine.start` via `attach_tile`
**Runs on**: CPU-pinned Flux tile thread
**File**: `auctioneer/mod.rs`

The central state machine. Receives all events via the `event` crossbeam channel
and processes them in a single-threaded loop (no locking needed):

- **Submissions** from SubWorkers: ranks bids, updates best bid per slot, dispatches
  simulation requests, gossips validated payloads to other relays
- **Simulation results**: marks bids as validated or demotes builders on failure
- **SlotUpdate/PayloadAttributes**: transitions auction state for new slots
- **GetHeader**: returns the current best bid for a slot/proposer
- **GetPayload**: returns the full execution payload for a signed header

Writes winning bids and submission traces to the DB via the block submissions
channel.

```
--[event crossbeam]--> this --[top_bid broadcast]---------> Top Bid WS
                             --[block_submissions mpsc]----> Block Submission Flusher
                             --[gossip (via gossiper)]-----> Other Relays
                             --{LocalCache}----------------> (updates best bids)
```

### API Layer

HTTP servers that accept external requests.

#### 9. API Server (:API_PORT)

**Spawned by**: `run_api_service`
**Runs on**: tokio (axum/hyper accept loop)
**File**: `api/service.rs`, `api/router.rs`

The main HTTP server. Accepts all external traffic:

- `POST /eth/v1/builder/blocks` — builder block submissions → `sub_worker` channel
- `POST /eth/v1/builder/validators` — validator registrations → `reg_worker` channel
- `GET /eth/v1/builder/header/:slot/:parent/:pubkey` — proposer get_header → `event` channel
- `POST /eth/v1/builder/blinded_blocks` — proposer get_payload → `event` channel (via SubWorker)
- `GET /relay/v1/data/*` — data API queries → DB directly

Each request is a short-lived tokio task spawned by the accept loop. The server
itself is the long-lived component.

```
Builders/Proposers ===[HTTP]==> this --[sub_worker crossbeam]--> SubWorkers
                                     --[reg_worker crossbeam]--> RegWorkers
                                     --[DB queries]------------> PostgreSQL
```

#### 10. Admin Server (:4050)

**Spawned by**: `start_admin_service`
**Runs on**: tokio (axum accept loop)
**File**: `api/admin_service.rs`

Minimal admin API behind bearer token auth. Currently supports a kill switch that
halts block submissions.

```
Admin ===[HTTP]==> this --{LocalCache kill_switch}--> API handlers (read on each request)
```

#### 11. Metrics Server (:9500)

**Spawned by**: `start_metrics_server`
**Runs on**: tokio (axum accept loop)
**File**: `common/src/metrics.rs`

Serves Prometheus metrics at `/metrics` and a health check at `/status`. All other
tasks write to a global `RELAY_METRICS_REGISTRY`; this server just reads and encodes
it on each scrape.

```
All tasks --{RELAY_METRICS_REGISTRY}--> this ===[HTTP]==> Prometheus
```

#### 12. Top Bid WebSocket (x N connections)

**Spawned by**: per-connection when a builder connects to the top bid WS endpoint
**Runs on**: tokio
**File**: `api/builder/top_bid.rs`

Not spawned at startup — created on demand when a builder opens a WebSocket
connection. Subscribes to the `top_bid` broadcast channel and pushes bid updates
to the connected builder. Sends ping every 10 seconds for keepalive.

```
--[top_bid broadcast]--> this ===[WebSocket]==> Builder
```

### Database Layer

Background tasks that batch and flush data to PostgreSQL.

#### 13. Registration Flusher

**Spawned by**: `start_registration_processor`
**Runs on**: tokio
**File**: `database/postgres/postgres_db_service.rs`

Every 2 seconds, drains the `pending_validator_registrations` DashMap and writes
them to PostgreSQL in a single batch. Registrations accumulate in the map from
RegWorker writes between flushes.

```
--{pending_validator_registrations DashMap}--> this ===[SQL]==> PostgreSQL
```

#### 14. Block Submission Flusher

**Spawned by**: `start_block_submission_processor`
**Runs on**: tokio
**File**: `database/postgres/postgres_db_service.rs`

Receives block submission records via a tokio mpsc channel. Batches them and
flushes every 5 seconds. Retries up to 3 times on failure. This keeps DB writes
off the Auctioneer's hot path.

```
--[block_submissions mpsc]--> this ===[SQL]==> PostgreSQL
```

#### 15. Known Validators Reload

**Spawned by**: `start_db_service` (non-submission instances only)
**Runs on**: tokio
**File**: `database/mod.rs`

Every 5 minutes (~1 epoch), reloads the full known validator set from PostgreSQL
into the in-memory cache. On submission instances, the Housekeeper handles this
instead.

```
PostgreSQL ===[SQL]==> this --{known_validators cache}--> API handlers
```

#### 16. DB Init Retry

**Spawned by**: `start_db_service`
**Runs on**: tokio (blocks startup until success)
**File**: `database/postgres/postgres_db_service.rs`

Not a background loop — runs at startup and retries database migrations every 1
second until they succeed. Blocks the rest of startup. Included for completeness
since it is technically a retry loop.

### Gossip Layer

Inter-relay communication for geographic distribution.

#### 17. gRPC Gossip Server (:50051)

**Spawned by**: `gossiper.start_server` inside `run_api_service`
**Runs on**: tokio (tonic accept loop)
**File**: `gossip/client.rs`

Listens for incoming gossip from other relay instances. Receives two message types:
`BroadcastPayload` (a validated block submission) and `BroadcastGetPayload` (a
request for a payload another instance needs). Forwards both to the `gossip` mpsc
channel.

```
Other Relays ===[gRPC]==> this --[gossip mpsc]--> Gossip Processor
```

#### 18. Gossip Processor

**Spawned by**: `run_api_service`
**Runs on**: tokio
**File**: `gossip/mod.rs`

Reads from the `gossip` mpsc channel and processes incoming gossip messages.
`BroadcastPayload` is fed into the BuilderApi (same path as a local submission,
but pre-validated). `BroadcastGetPayload` is fed into the ProposerApi to attempt
payload delivery.

```
--[gossip mpsc]--> this --> BuilderApi::process_gossiped_payload
                        --> ProposerApi::process_gossiped_get_payload
```

#### 19. Gossip Client Connect (x N relays)

**Spawned by**: `GrpcGossiperClientManager::new`
**Runs on**: tokio
**File**: `gossip/client.rs`

One per configured remote relay. Retries gRPC connection with exponential backoff
(1s → 60s max) until connected. Once connected, the task exits and the client
is stored for use by the Auctioneer when it needs to broadcast.

Not a permanent loop — terminates once connected. But it runs for an unbounded
time if the peer is down.

```
this ===[gRPC connect]==> Remote Relay
```

### Network Layer

WebSocket-based relay-to-relay protocol (separate from gRPC gossip). Used for
inclusion list consensus between geographically distributed instances.

#### 20. Peer Message Handler (x N peers)

**Spawned by**: `RelayNetworkManager::new`
**Runs on**: tokio
**File**: `network/message_handler.rs`

One per configured peer (where `peer_pubkey > our_pubkey` to avoid duplicate
connections). Outer loop connects with exponential backoff. Inner loop runs
a `tokio::select!` over:
- Incoming WebSocket messages from the peer
- Outgoing messages from the `network_broadcast` channel

Messages are signed and verified using relay BLS keys.

```
--[network_broadcast broadcast]--> this ===[WebSocket]==> Peer Relay
Peer Relay ===[WebSocket]==> this --[api_events mpsc]--> Network Event Handler
```

#### 21. Network Event Handler

**Spawned by**: `RelayNetworkManager::new`
**Runs on**: tokio
**File**: `network/event_handlers.rs`

Processes network events from the `api_events` mpsc channel. Currently handles
inclusion list consensus: collects inclusion lists from peers, merges them, and
returns the result via a oneshot channel to the caller (Housekeeper).

```
--[api_events mpsc]--> this --[oneshot]--> Housekeeper (caller)
```

### Simulation

#### 22. Simulator Health Check

**Spawned by**: `SimulatorManager::new` (inside Auctioneer init)
**Runs on**: tokio
**File**: `auctioneer/simulator/manager.rs`

Polls each configured block simulation backend (Reth instances) for sync status
every 2 seconds via gRPC. Updates the simulator's availability so the Auctioneer
only dispatches to healthy simulators.

```
Simulators ===[gRPC]==> this --{simulator sync state}--> Auctioneer (reads on dispatch)
```

#### 23. Simulator DB Health Check

**Spawned by**: `Context::new` (inside Auctioneer init)
**Runs on**: tokio
**File**: `auctioneer/context.rs`

Periodically checks database connectivity and updates a flag that controls whether
the Auctioneer will accept submissions. If the DB is unreachable, submissions are
rejected to avoid data loss.

```
PostgreSQL ===[health check]==> this --{db_available flag}--> Auctioneer
```

### Maintenance

#### 24. Rate Limiter Cleanup

**Spawned by**: `build_router`
**Runs on**: std::thread (not tokio)
**File**: `api/router.rs`

A standard OS thread (not a tokio task) that cleans up expired entries from the
per-IP rate limiter every 60 seconds. Uses a std thread because the rate limiter
is also accessed from synchronous middleware.

```
this --{rate limiter state}--> API middleware (per-request check)
```

#### 25. Relay Data Stats Reset

**Spawned by**: `RelayDataApi::new`
**Runs on**: tokio
**File**: `api/relay_data/api.rs`

Resets in-memory counters for delivered blocks and builder blocks every 60 seconds.
These counters are used by the data API to serve recent activity stats.

```
this --{stats counters}--> Data API handlers (read on request)
```

### Website

#### 26. Website Service

**Spawned by**: `spine.start` callback (if `website.enabled`)
**Runs on**: tokio
**File**: `website/website_service.rs`

Outer loop restarts the service on crash. Inner loop watches for slot changes and
re-renders website templates with updated data (recent blocks, builder stats).
Serves the relay's public-facing website.

```
--{slot changes}--> this ===[DB queries]==> PostgreSQL
                         ===[HTTP]==> (serves website)
```

## Data Flow Summary

The critical path through the system:

```
                                   RegWorkers (CPU-pinned)
                                  /         \
Builder ---HTTP---> API Server --+           +--> DB Flush
                                  \         /
                                   SubWorkers (CPU-pinned)
                                       |
                                       | event channel
                                       v
Beacon SSE --> ChainEventUpdater --> Auctioneer (CPU-pinned)
           --> Housekeeper -------/      |
                                         +---> Top Bid WS --> Builders
                                         +---> Gossip ------> Other Relays
                                         +---> DB Flush -----> PostgreSQL
                                         |
                                         v
Proposer ---HTTP---> API Server --> SubWorker --> Auctioneer --> Gossip
                                                     |
                                                     v
                                               Beacon Node (broadcast)
```
