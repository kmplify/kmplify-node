# The LAN router and `kmplify-node gui`

kmplify-node's first job is to lend one machine *outward*, to a fabric, over a
single outbound socket. The router points the other way: it turns the
kmplify-nodes on one local network into a personal inference cluster. Every
machine finds the others, shows what each can serve, and (from phase 2) an
application on any of them gets one endpoint that routes each request to
whichever machine is best placed to answer it. Prompts and answers travel
between machines you own and never leave the network.

The design is adapted from NVIDIA's Personal AI Router (PAIR), Apache-2.0,
see [NOTICE](../NOTICE). PAIR is an Electron application over thirteen Go
services talking JSON-RPC over stdio. This is the same product shape inside
one Rust binary, on Linux, macOS and Windows, and it reuses what this crate
already had: engine detection, GPU and CPU probes, the settings file, the
status snapshot and the fabric worker.

## Run it

```bash
cargo build --release --features gui
kmplify-node gui
```

`gui` behaves like `tui`: it **attaches** to the fabric node already running
here, or **starts** one and stops it when the window closes. `--attach` and
`--standalone` force either. The window's fabric side (link state, delivered
jobs, the sharing settings) is that node; the router side (discovery, meters,
routing) lives in the window's own process.

The `gui` feature is off by default. A headless service binary has no use for
a window toolkit, and the feature pulls one in. `router` alone (mDNS, no
window) is a separate feature for a future headless router mode.

## What this changes about the crate's promises

The fabric worker never listens on a port, and that stays true: nothing in
`kmplify-node`, `run`, `tui`, `check` or `set` binds anything. The router is
a separate, opt-in mode, and only it will:

- send and receive **multicast DNS on the local link** (this phase);
- bind the **node-info surface** and the **two routing proxies** on this
  machine's interfaces (phase 2), loopback for local applications, mutual
  TLS for paired peers, nothing for anyone else.

Everything the router puts on the network is listed on its settings screen:
hostname, node id, GPU model, core and memory counts, which engines answer
and how many models each serves. Never a model name in the announcement,
never a request, never a response. Nothing is logged that a request
contained; the jobs column shows model, engine, origin, destination and time,
exactly as PAIR's does, and there is no switch that adds more.

This is local-first and stays local-first. No part of the router talks to
`fabric.kmplify.io` or any other cloud; a machine with the router on and the
fabric worker paused is a purely on-premises cluster.

## The mapping

| PAIR component | Here | Phase |
|---|---|---|
| `nvpair-node-scanner` (mDNS, directory, enrichment, eviction) | `router::discovery` + `Router::expire` | 1 (done) |
| `nvpair-node-info` (GPU/CPU/RAM telemetry, `/v1/node-info`) | `router::telemetry` (local sampling) · HTTP surface | 1 · 2 |
| `nvpair-engine-manager` (detect, inventory) | `engines::scan` via `telemetry::scan_engines` | 1 (detect, inventory) · 3 (install, start, stop, pull) |
| `nvpair-manual-nodes` | `Router::manual` + probing | 1 (list) · 2 (probe) |
| `nvpair-node-settings` | `settings::Settings` (existing) | 1 |
| `ollama-proxy`, `lmstudio-proxy` (routing, owner failover, fan-out `/v1/models`) | `router::proxy` | 2 |
| `nvpair-job-scheduler` (pending + smoothed GPU pressure) | `router::schedule` | 2 |
| `nvpair-workload-manager` (jobs replicated cluster-wide) | `Router::jobs` + peer replication | 1 (local) · 2 (replicated) |
| `nvpair-cluster-manager` + `eap-noob` (identity, PIN pairing, pinned certs, mTLS) | `router::cluster` | 3 |
| `nvpair-errors` | log ring + peer sync | 1 (local) · 3 |
| `nvpair-ui-broker` (supervision, JSON-RPC relay) | not needed: one process, one `Router` behind a mutex | — |
| `nvpair-tui` | `kmplify-node tui` gains the router screens | 3 |
| Electron `desktop/` | `src/gui` (egui, native) | 1 (Overview, Jobs, Settings, Endpoints, Add node) · 2 (Chat) · 3 (Model hub, Welcome, Tray) |

Ports, chosen apart from the engines' defaults and from PAIR's `143xx` band
so both can run on one machine:

| Port | Listener | Phase |
|---|---|---|
| `14418` | node-info: hardware, meters, model inventory (plain HTTP, LAN) | 2 |
| `11440` | Ollama-compatible routing proxy | 2 |
| `11441` | OpenAI-compatible routing proxy | 2 |

PAIR takes over the engines' own ports (`11434`, `1234`) so unmodified clients
gain the cluster. That is a real convenience and a real surprise; this router
does not move an engine anyone installed. An application opts in by pointing
at the router's port, and the Endpoints window is where it copies that from.

## Phase 1 — what is on this branch

- `router::Router`: the shared state every screen draws from. Nodes keyed by
  id (never hostname), a minute of history per measurement, jobs newest
  first, the manual list, a log ring.
- `router::telemetry`: this machine sampled once a second (CPU, RAM, GPU
  load, VRAM) from the crate's existing probes; the engine roster rescanned
  every ten seconds; quiet peers expired.
- `router::discovery`: one `_kmplify-node._tcp` record per host, identity
  and static facts in TXT, re-registered only when they change; the browse
  side folds peers into the directory.
- `kmplify-node gui`: Overview with a card per node (radial ring, engine
  badges, a smoothed area chart for GPU, VRAM, CPU and RAM with a click-to-
  solo legend), the jobs column with filters, Settings (the same file `set`
  writes, plus pause/resume/reconnect for a node this window started),
  Endpoints, Add node.

Fabric jobs appear in the jobs column as they finish, from the node's own
counters; nothing about their content is available to the window, by design.

## Phase 2 — routing

- `router::node_info`: serve `GET /v1/node-info` (hardware, meters, per-engine
  inventory, loaded models) on `14418`; the discovery side fetches it for
  each peer every two seconds with backoff on failure, so a peer's card gets
  live meters and model names. Manual nodes are probed the same way.
- `router::proxy`: the two compatibility proxies. Model-bearing requests go
  only to nodes whose inventory advertises the model (Ollama's implicit
  `:latest` normalised; LM Studio ids exact); `/v1/models` and `/api/tags`
  fan out and merge; `404` from an advertised owner fails over to the next,
  `4xx` client errors do not. Streaming passes through untouched.
- `router::schedule`: pending work per node plus the busiest GPU smoothed
  into pressure 0–3 (40/70/85 % up, lower on the way down), neutral 1 when
  telemetry is missing or older than ten seconds; ranking published only
  when it changes; per-proxy reservations so a burst spreads.
- Jobs replicated between nodes so every window shows work running anywhere.
- Chat: a pane that talks to the router's own endpoint.

## Phase 3 — trust and lifecycle

- Pairing with a six-digit PIN (PAIR uses EAP-NOOB), pinned self-signed leaf
  certificates per node, mutual TLS on every peer surface but pairing and
  telemetry, a cluster id, leave and remove.
- Engine lifecycle: install, start, stop, update Ollama and LM Studio;
  adopt an instance the user started; model pulls with progress.
- Terminal screens for the same, so a headless router is operated fully.
- Packaging: `.deb`, `.dmg`, Windows installer, autostart, tray.

## Compliance notes

- Local-first by construction: no cloud endpoint is involved in routing.
- Data minimisation on the wire: the announcement carries capacity, not
  content; the proxies carry requests, never store them, and log only
  operational metadata.
- The router is opt-in per machine and says on its settings screen exactly
  what a neighbour on the subnet can learn from it.
- Nothing here identifies a person: the node id is the fabric's anonymous
  id, or a hash of the hostname when the machine never joined a fabric.
