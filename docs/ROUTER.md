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
jobs, the sharing settings) is that node.

The router side (discovery, meters, routing, the cluster) is a service of
the node in the same sense: it runs in exactly one process on the machine,
and every view attaches to it. `kmplify-node run --router` runs it
headless; `kmplify-node gui` and `kmplify-node tui --router` start it only
when no process runs it yet, and otherwise draw from `router.json` (written
once a second by the owning process) and leave their orders as files in
`control/router/` (`router::snapshot`, `control::RouterCommand`). The
operator can therefore switch between the console view and the window at
any time, and the router is not a property of whichever screen is open.
`--standalone` forces both node and router into the current process.

The `gui` feature is off by default. A headless service binary has no use for
a window toolkit, and the feature pulls one in. `router` alone (no window)
is what `run --router` and `tui --router` need, and the release binaries
for macOS and Windows carry `gui`; the static Linux ones are headless and
the `.deb` has the window.

## What this changes about the crate's promises

The fabric worker never listens on a port, and that stays true: nothing in
`kmplify-node`, `run`, `tui`, `check` or `set` binds anything. The router is
a separate, opt-in mode, and only it:

- sends and receives **multicast DNS on the local link**;
- binds the **node-info surface** (`14418`) and the **two routing proxies**
  (`11440`, `11441`) on this machine's interfaces.

Who may talk to the proxies is decided per connection:

| Caller | Treatment |
|---|---|
| this machine, on loopback | routed anywhere in the cluster |
| a paired node, over mutual TLS, while *LAN ingress* is on | served by this node's own engine, never routed on |
| anything else — plaintext from the network included | `403` |

A request only ever goes *out* to a paired node too, over the same mutual
TLS. The node-info surface follows a slightly different rule so that two
strangers can find each other before they pair: while a node is in no
cluster its report is plain HTTP readable by anything on the subnet (the
trade PAIR makes for its telemetry); once it is in a cluster, only loopback
and paired nodes over mutual TLS may read it. Pairing itself is plaintext,
authenticated by the PIN. All of this is stated on the settings screen,
which is also where LAN ingress is switched off to make a machine a pure
consumer of the cluster.

## Trust: certificates, pairing, mutual TLS

Every node mints one self-signed certificate on first start
(`<node dir>/router/node.crt.der`, `node.key.der`, owner-only) and keeps
it: the fingerprint peers pin would change otherwise and every pairing
would be undone. A **cluster** is the set of nodes that have pinned each
other's fingerprints, in `router/cluster.json`, plus a cluster id and a
tombstone list of nodes removed on purpose. There is no authority and no
primary; membership is symmetric.

**Pairing** (Settings → Cluster) is one machine showing a six-digit PIN and
another typing it with the first machine's address:

1. The joiner sends its certificate and a SPAKE2 message to
   `POST /v1/pair` (plaintext, port 14418) on the inviter.
2. The inviter, holding an open invitation, finishes the SPAKE2 exchange
   under the PIN, answers with its own certificate and message, the cluster
   id, and an HMAC over the transcript (both fingerprints and the cluster
   id) under the agreed key.
3. The joiner finishes the exchange, checks that HMAC — a wrong PIN on
   either side gives a different key, so this is where a mismatch shows —
   and sends its own HMAC.
4. The inviter checks it, pins the joiner, and answers with the whole
   member list; the joiner pins everyone. Each side opens a card for the
   other at the address the exchange came from and polls it at once.

SPAKE2 is what makes a six-digit PIN sufficient: a listener on the network
gets nothing it can brute-force offline, and an active man in the middle
gets one online guess per attempt. An invitation allows three wrong PINs
and lasts five minutes, then closes. PAIR uses EAP-NOOB for the same step.

**Mutual TLS** is then used on every peer surface. Each side presents its
certificate and checks the other's fingerprint against its pins, nothing
else: no CA, no hostname check, no clock. A node learns about members it
did not pair with directly from any member's node-info report — accepted
only when that report arrived over mutual TLS and names the same cluster,
and never for a node on the tombstone list. Leave drops every pin; Remove
drops one and tombstones it.

One port carries both personalities: a connection whose first byte is
`0x16` is a TLS handshake and goes to the acceptor, anything else is
plaintext HTTP. The handler learns the caller's address and, for a pinned
handshake, its node id, and decides from that.

Two environment overrides exist for running two nodes on one machine (a
test) or naming a machine: `KMPLIFY_ROUTER_PORTS=info,ollama,openai` and
`KMPLIFY_NODE_NAME`.

Everything the router puts on the network: hostname, node id, hardware,
which engines answer and their model names, how busy the machine is, which
jobs ran where. Never a request, never a response. Nothing is logged that a
request contained; the jobs column shows model, engine, origin, destination
and time, exactly as PAIR's does, and there is no switch that adds more.

This is local-first and stays local-first. No part of the router talks to
`fabric.kmplify.io` or any other cloud; a machine with the router on and the
fabric worker paused is a purely on-premises cluster.

## The mapping

| PAIR component | Here | Phase |
|---|---|---|
| `nvpair-node-scanner` (mDNS, directory, enrichment, eviction) | `router::discovery` + `node_info::poll_peers` + `Router::expire` | done |
| `nvpair-node-info` (GPU/CPU/RAM telemetry, `/v1/node-info`) | `router::telemetry` (sampling) + `router::node_info` (surface and polling) | done |
| `nvpair-engine-manager` (detect, inventory, install, start, stop, pull, remote control) | `engines::scan` via `telemetry::refresh_roster` + `router::engine` + `POST /v1/engine` | done |
| `nvpair-manual-nodes` | `Router::manual` + `node_info::poll_peers` (re-keyed to the real id on first answer) | done |
| `nvpair-node-settings` | `settings::Settings` (existing) | done |
| `ollama-proxy`, `lmstudio-proxy` (routing, owner failover, fan-out listings) | `router::proxy` | done |
| `nvpair-job-scheduler` (pending + smoothed GPU pressure) | `router::schedule` | done |
| `nvpair-workload-manager` (jobs replicated cluster-wide) | `Router::jobs`, carried in every node-info report and merged by id | done |
| `nvpair-cluster-manager` + `eap-noob` (identity, PIN pairing, pinned certs, mTLS) | `router::cluster` (SPAKE2 pairing, pinned-fingerprint verifiers) + `router::listen` (one port, two personalities) | done |
| `nvpair-errors` | log ring + peer sync | local done · 3 |
| `nvpair-ui-broker` (supervision, JSON-RPC relay) | not needed: one process, one `Router` behind a mutex | — |
| `nvpair-tui` | `kmplify-node tui --router`: screens 8 network and 9 cluster | done |
| Electron `desktop/` | `src/gui` (egui, native) | Overview with connection lines, Jobs, Settings, Endpoints, Add node, Chat, engine panel, tray, autostart: done. PAIR's Model hub is the engine panel's pull; its Welcome screen is `kmplify-node init` |
| Electron packaging (dmg, exe, AppImage) | `cargo deb`, `scripts/bundle-macos.sh`, `packaging/windows/kmplify-node.iss`, all driven by `release.yml` | done |

Ports, chosen apart from the engines' defaults and from PAIR's `143xx` band
so both can run on one machine:

| Port | Listener |
|---|---|
| `14418` | node-info: hardware, meters, engines with model names, pending work, recent jobs (plain HTTP, LAN) |
| `11440` | Ollama-compatible routing proxy (`/api/*`, and the `/v1/*` Ollama serves) |
| `11441` | OpenAI-compatible routing proxy (`/v1/*`, every engine) |

### `GET /v1/node-info`

```json
{
  "id": "9a708f66…", "name": "King", "version": "0.6.1+…",
  "gpus": [{"name": "NVIDIA GeForce RTX 4090", "total_mb": 24564, "used_mb": 1950, "utilization_percent": 23}],
  "cpu": {"model": "13th Gen Intel(R) Core(TM) i9-13900K", "cores": 32, "utilization_percent": 7.8},
  "memory": {"total_mb": 64832, "used_mb": 41600},
  "engines": [{"id": "ollama", "name": "Ollama", "base": "http://127.0.0.1:11434", "models": ["qwen3:14b", "…"], "running": true}],
  "sampled": true, "vram_known": true,
  "pending": 0, "proxy_ports": [11440, 11441], "lan_ingress": true,
  "jobs": [{"id": "…", "model": "qwen2.5:7b-instruct", "engine": "ollama", "requested_from": "King", "ran_on": "King", "node_id": "…", "state": "completed", "at_ms": 0, "error": ""}]
}
```

`utilization_percent` fields are absent where the platform gives no
reading (Apple Silicon exposes GPU load to privileged tools only), and a
consumer charts nothing rather than a zero. `jobs` carries the last fifty;
a peer merges them by id, so every window shows work running anywhere.

### How a request is routed

1. The caller is classified (table above). A peer's request, or one carrying
   `X-KMPLIFY-Hop`, is served by this node's own engine only.
2. `GET /api/tags` and `GET /v1/models` fan out to every online node's
   engines concurrently and merge by name, first answer keeping the
   metadata and every owner listed under `kmplify_nodes`.
3. Otherwise the `model` is read from the JSON body. The candidates are the
   online nodes whose *running* engine advertises it — Ollama's implicit
   `:latest` normalised, other ids exact — and `router::schedule` orders
   them: pending work plus GPU pressure, then pressure, then id.
4. Candidates are tried in order. This machine's engine is called directly;
   a peer through its proxy with the hop header. A `404` (stale inventory)
   or `5xx` moves to the next owner; a `400`/`422` is returned as is. The
   response streams back untouched.
5. A job card is opened at dispatch and closed when the response body ends,
   which for a streamed generation is the only moment it is finished.

No candidate at all is an immediate `502` naming the model; the request is
never sent to an engine that does not have it.

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

## Phase 2 — routing (on this branch)

- `router::node_info`: `GET /v1/node-info` on `14418`, and the poll of every
  peer's copy: two seconds while it answers, 4/8/16/30 while it does not,
  last good value kept. A node added by address is re-keyed to its real id
  on its first answer; typing this machine's own address adds nothing.
- `router::proxy`: the two compatibility proxies, routing as described
  above, with the caller gate and the hop rule.
- `router::schedule`: pending work per node (what it reports, or what this
  machine has dispatched to it and not yet seen reported — the reservation
  that spreads a burst) plus the busiest GPU smoothed into pressure 0–3
  (40/70/85 % up, ten points lower on the way down, one step per sample),
  neutral 1 when the GPU sample is missing or older than ten seconds.
  Computed at request time from the shared state; nothing to publish.
- Jobs travel in node-info reports and merge by id.
- Chat: a pane on the router's own OpenAI endpoint with a model picker fed
  by the network's inventory, so a message typed there is routed exactly
  like an application's request and shows up in the jobs column.
- LAN ingress switch on the settings screen.

Verified on one Windows node: node-info answers with live meters; the two
listings merge Ollama's and LM Studio's inventories (5 + 8 models); a chat
completion and a native `/api/generate` route to the local engine and appear
as completed jobs; an unknown model is a `502` naming it; a request from a
non-loopback, non-directory address is a `403`. Two-node routing, failover
and the scheduler's ordering are unit-tested but have not yet run against a
second physical machine.

## Phase 3 — trust and lifecycle

Done on this branch: pairing with a six-digit PIN over SPAKE2, one pinned
self-signed certificate per node, mutual TLS on every peer surface but
pairing, the cluster id, member replication through trusted reports, leave
and remove with tombstones, and the cluster card in Settings.

Verified with two nodes on one Windows machine (a second instance under
`KMPLIFY_ROUTER_PORTS` and `KMPLIFY_NODE_NAME`, pointed at a dead gateway):
an invitation on one, the PIN typed on the other; both ended with the
other's fingerprint pinned and the same cluster id; each polled the other
over mutual TLS and drew its live meters; `/v1/models` through one node's
proxy listed both nodes as owners of every model, fetched from the peer
over TLS; a chat completion routed and completed; a plaintext request from
the LAN address to a proxy was refused with `403`, and so was a plaintext
node-info read once the node was clustered. mDNS does not let two nodes on
one host see each other, so that path used the address typed at pairing.

### Engine lifecycle (`router::engine`)

Also on this branch: install, start, stop and model pulls, from a node's
own window or from a paired node over mutual TLS (`POST /v1/engine` on the
node-info surface, PAIR's cluster-scoped engine control). PAIR's two rules
are kept whole:

- **Find before install.** Detection looks on PATH, in the places the
  official installers use, and in the router's own
  `<node dir>/router/engines/`; an engine the operator installed is used
  where it is, and "install" on such a machine downloads nothing.
- **Adopt, never seize.** An instance already serving its port is used and
  reported as running, but this node did not start it, so it will not stop
  it. Only a process this node spawned is *owned*, and only an owned one
  can be stopped from here. LM Studio is the exception PAIR also makes: it
  publishes an official stop (`lms server stop`), so an adopted instance
  can be stopped too.

Ollama is installed from its official release archive for the platform
(Windows zip, Linux and macOS tarballs), extracted with the `tar` every
supported platform ships, and started with `OLLAMA_HOST` on loopback so
the proxy stays the only network-facing listener. Pulls stream Ollama's
`/api/pull` progress. LM Studio is a desktop application with its own
installer; once installed, its `lms` command starts, stops and downloads
for it. Every step is an operation on the node's card, with byte progress
where there is a size, carried in node-info reports so a paired node's
window shows it too.

Verified here: start on a machine whose Ollama was already running was
reported as adopted; stop of that adopted instance was refused with the
reason; install found the existing binary and downloaded nothing; a pull
streamed progress to done and the model appeared in the inventory; LM
Studio's server was stopped and started again through `lms`; an engine
request from an unpaired LAN address was refused. The download path of
`install` has not been exercised on a machine without Ollama.

### Terminal screens (`kmplify-node tui --router`)

A router node usually runs where there is no desktop. `--router` starts
the router in the dashboard's process (needs a build with the `router`
feature; without one, or without the flag, the screens say so) and adds two
screens in the dashboard's idiom:

- **8 network**: every node with its state (LOCAL, PAIRED, UNPAIRED, OTHER
  cluster, OFFLINE), address, GPU, the engines answering with their model
  counts, CPU and GPU load and pending work; the routed jobs; the
  endpoints and the discovery and listener state. `a` adds a node by
  address.
- **9 cluster**: this node's certificate fingerprint, the cluster id, the
  members with online marks and fingerprints; `i` opens an invitation and
  shows its PIN with this machine's address and the time left, `n` cancels
  it; `o` joins — the address is asked for, then the PIN, which is never
  echoed; `d` removes the selected member and `L` leaves, both after a
  `y`.

Verified here with the dashboard in one window and a second node in the
desktop window: the network screen listed both nodes as LOCAL and PAIRED
with their engines and meters, a node added by address through `a` was
re-keyed to its pinned identity (its plaintext report was refused, the
retry over mutual TLS succeeded), its address was persisted for the next
start, the fan-out listing named both nodes, and `i` showed a PIN. The
screens are also rendered into ratatui's test backend by unit tests that
check their content and key handling.

Four robustness changes came out of this, all in how a peer's address is
chosen and kept:

- a member's last known address and node-info port are persisted in
  `cluster.json` and its card recreated at start;
- a member that reaches this node over the cluster's TLS gets a card at
  that address if none exists, and moves a failing card there, so
  membership stays symmetric after either side restarts;
- of the addresses a node announces (its LAN, a link-local leftover, the
  virtual adapters of WSL, Hyper-V or Docker) the one on this machine's
  own /24 is preferred, then any routable IPv4, and an announcement never
  moves a card whose polls are succeeding;
- the announced SRV port is the port a peer is polled on, rather than
  this process's own — which is what let two nodes on one machine, on
  different ports, reach each other in both directions.

### One router, any number of views (`router::snapshot`)

The router runs in one process per machine and publishes `router.json`
(the whole `Router` less its live handles: nodes with their history,
jobs, cluster, invitation, log) once a second, owner-readable. A view
opened while another process runs it attaches through `RouterHandle`:
`view()` reads the file (cached for a second), `command()` writes a
`RouterCommand` file into `control/router/`, which the owning process
drains every half second and applies through the same `apply` the local
handle uses. Orders are the same nine verbs in both modes: invite, cancel,
join, add and forget a node, remove a member, leave, LAN ingress, and an
engine action on any node. The directory is separate from `control/` on
purpose: the node process drains that one, and the router may live in a
different process (a window attached to a headless node), so an order left
there would be eaten by the wrong reader.

Fabric jobs become router job cards in the owning process too
(`telemetry::mirror_fabric_jobs`), from the node's counters, and only when
that process is the node, so an attached window does not double-count.

Verified here with the installed node running headless in one terminal,
the window started second (router in the window), and `tui --router`
started third: the dashboard's footer said attached to both; `i` on its
cluster screen opened an invitation that the window's router logged and
showed; `n` cancelled it; a chat through the proxy appeared as a job in
both views with the connection line drawn in the window; and with
`engine=http://127.0.0.1:11440` a fabric job pinned to this node by
invitation ran through the router (the node's counter and the router's
job list both moved), after which the setting was put back.

### Packaging

- **Debian/Ubuntu**: `[package.metadata.deb]` in `Cargo.toml` for
  `cargo deb`: the binary with the window, `packaging/kmplify-node.desktop`,
  the icon, the systemd unit (installed, not enabled: it runs as the
  `kmplify` user the operator creates), the man page
  (`packaging/kmplify-node.1`), the Debian changelog
  (`packaging/debian/changelog`, one entry per version) and the docs. Built for amd64 and arm64.
- **macOS**: `scripts/bundle-macos.sh` wraps the binary into
  `KMPLIFY Node.app` (a two-line launcher runs it with `gui`; the binary
  inside stays the full CLI), renders the `.icns` from
  `packaging/icons/kmplify-node-1024.png`, signs ad hoc and writes the
  `.dmg`. `packaging/macos/io.kmplify.node.plist` is a LaunchAgent for a
  headless `run --router` at login.
- **Windows**: `packaging/windows/kmplify-node.iss` for Inno Setup (on the
  hosted runners): per-user by default, Start menu entries for the window
  and the dashboard, optional PATH, desktop shortcut and sign-in autostart
  (an HKCU Run value), and the uninstaller stops a window hidden in the
  tray first.
- **Icon**: one geometry (`src/gui/icon.rs`) rasterised at runtime for the
  window and tray icons, and the same geometry rendered to
  `packaging/icons/` (PNG, ICO) for the packages.
- **Tray and autostart** (`src/gui/tray.rs`, `src/gui/autostart.rs`): on
  Windows and macOS the window hides behind a tray icon while it hosts the
  node or the router, and the tray menu reopens or quits it; the settings
  screen's "open when I sign in" writes and removes the per-user autostart
  entry on all three platforms. Linux has no tray here (the crate needs
  GTK and an appindicator library, and the desktops disagree), so the
  window closes and a `run --router` service is the way to keep the router
  up.
- `release.yml` builds all of it per tag; `ci.yml` builds and tests the
  `gui` feature on the three desktops.

Not exercised on this machine: the `.deb` and `.dmg` themselves (no
Debian or macOS here), the Inno Setup compile (no `iscc` installed
locally), and mDNS discovery between two physical machines.

## Compliance notes

- Local-first by construction: no cloud endpoint is involved in routing.
- Data minimisation on the wire: the announcement carries capacity, not
  content; the proxies carry requests, never store them, and log only
  operational metadata.
- The router is opt-in per machine and says on its settings screen exactly
  what a neighbour on the subnet can learn from it.
- Nothing here identifies a person: the node id is the fabric's anonymous
  id, or a hash of the hostname when the machine never joined a fabric.
