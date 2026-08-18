# KMPLIFY GPU Fabric protocol

The wire contract between a **node** (this repository) and a **gateway**.
Published so a provider can verify what their machine agreed to, and so
anyone can write their own node or their own gateway. The gateway
implementation is not part of this repository; this document is the whole
of what it may ask a node to do.

The fabric connects **provider nodes** (kmplify-desktop installs sharing
their GPU) with **consumers** (rag-system deployments, desktop nodes in
`peer` mode, the portal). Nodes are anonymous and dial OUT — nothing on a
provider machine is ever reachable from the internet.

## Roles

```
Consumer ──HTTP──► Gateway ◄──WebSocket (outbound)── Provider node (worker)
   OpenAI /v1          job queue + registry              local Ollama
```

## Node lifecycle

1. `POST /fabric/register` → `{ "node_id": "...", "token": "..." }`
   Anonymous: no account, no PII. The worker persists the token locally.

   **Identity continuity (v2.3):** a re-registering worker may send
   `{ "previous_node_id": ..., "previous_token": ... }`. If the gateway has
   no record of that node_id (registry lost/reset) it **re-adopts** the old
   id under a fresh token; if the id is known and the token matches, the id
   is kept and the token rotated. Only a known id with a wrong token falls
   through to a brand-new identity (anti-hijack). Invitations are bound to
   the provider's node_id, so continuity is what lets a consumer's stored
   invitation UUID reconnect automatically after the provider or gateway
   comes back. Pre-v2.3 workers send no body — unchanged behaviour.
2. Worker opens `WS /fabric/connect`, first frame:
   `{ "type": "hello", "node_id": ..., "token": ..., "models": ["llama3:8b", ...],
      "gpu": { "backend": "metal|cuda|cpu", "name": "...", "vram_mb": 0 },
      "country": "DE" }`
   Gateway replies `{ "type": "welcome" }` or closes with 4001 (bad auth).

   `country` is an optional ISO-3166-1 alpha-2 code for where the machine
   physically is. Anything absent or malformed is stored as `"XX"`.

   **It is self-reported and the gateway cannot verify it.** It exists so
   consumers can *prefer* EU/EEA capacity (`X-KMPLIFY-Region: eu`), which is a
   data-residency preference — not an attestation, and not on its own a DSGVO
   compliance guarantee. Surfaces that expose it must say so. Requests that
   ask for EU fail closed: `"XX"` is never treated as EU.
   **`worker_version` (v2.7).** The build of THIS crate, always, alongside
   `version`. When a host application embeds the worker (the KMPLIFY desktop
   app), `version` reports the host's own release and therefore cannot say
   which worker is inside it: two installs reporting an identical `version`
   may carry different protocol support. `worker_version` answers that
   directly. Sent as its own key rather than folded into `version` because
   gateways truncate that field to 32 characters; gateways that do not know
   the key ignore it.

3. Heartbeat: gateway sends `{"type":"ping"}` every 20s; worker answers
   `{"type":"pong"}`. Two missed pongs → node marked offline, in-flight jobs
   rescheduled to other nodes.

## Data residency (consumer side)

`POST /v1/chat/completions`, `/v1/embeddings` and `/v1/workloads` accept an
optional `X-KMPLIFY-Region: eu` header. When set, the scheduler only considers
nodes that declared an EU/EEA `country`; if none is online it returns 503 with
a message distinguishing "nobody serves this model" from "nobody in the EU
does". A header is used rather than a body field because the request body is
forwarded verbatim to the serving node's Ollama.

`GET /fabric/nodes?eu_only=true` applies the same filter to the dashboard view.

## Jobs

Gateway → worker:
`{ "type": "job", "id": "...", "kind": "chat" | "embeddings", "payload": <verbatim OpenAI request> }`

Worker → gateway (streaming chat):
- `{ "type": "chunk", "id": ..., "data": <OpenAI chunk object> }` (0..n)
- `{ "type": "done",  "id": ..., "data": <final OpenAI response | null> }`
- `{ "type": "error", "id": ..., "message": "..." }`

The worker forwards payloads verbatim to the local Ollama OpenAI endpoint
(`/v1/chat/completions`, `/v1/embeddings`), so any model the node serves
works without fabric-side model knowledge.

## Consumer API

OpenAI-compatible, so every existing client just points its base_url here:
- `POST /v1/chat/completions` (stream and non-stream)
- `POST /v1/embeddings`
- `GET  /v1/models` — union of all online nodes' models
- `GET  /fabric/nodes` — anonymized node list (id prefix, models, GPU class,
  jobs served, reliability) for the portal dashboard

Scheduling: filter online nodes serving the requested model → pick the one
with the fewest active jobs. On node drop mid-job: error to the consumer in
v0 (retry lands in v1 together with result sampling).

Consumer auth: `FABRIC_API_KEY` env — when set, `Authorization: Bearer` is
required; unset = open (local/dev).

### One connection per node_id (v2.4)

`node_id` identifies a MACHINE; a WebSocket identifies a connection. The
gateway keeps exactly one connection per node_id — the newest — because a
worker that drops and redials must be able to take over immediately without
waiting out the ping timeout.

When a new connection displaces an existing one the gateway closes the old
socket with **`4003`** (distinct from `4001` bad credentials and `4002` no
pong). Workers should treat it as a normal disconnect and reconnect with
their usual backoff. Before this, the displaced socket stayed open and
received nothing: its worker went on answering pings, believing it was
sharing, while the scheduler — which only sees the newest connection — never
sent it work.

In-flight jobs are failed per CONNECTION, not per node_id, so a fast
reconnect no longer has its fresh jobs killed by the previous connection's
cleanup.

If the displaced connection was still answering pings, two live processes
hold the same credentials — a cloned VM image, a copied `fabric_node.json`,
or a headless node running beside the desktop app. The gateway cannot
arbitrate (both present valid tokens for one identity), so it logs
`DUPLICATE IDENTITY` naming the node id; give one of them its own identity
by deleting its credential file and letting it re-register.

## Per-model engines — colibri MoE streaming (v2.5)

A worker may run a **second local upstream** next to Ollama: a
[colibri](https://github.com/JustVugg/colibri) `coli serve` gateway
(`COLIBRI_BASE`, optional `COLIBRI_API_KEY`). Colibri executes frontier MoE
models (284B–2.8T parameters) by treating storage, RAM and VRAM as one
placement hierarchy — experts stream from NVMe on demand — so the models a
provider can lend are **no longer bounded by its VRAM**, and a CPU-only
node can serve a 744B model.

Protocol surface (all backward compatible):

- `hello` and the `models` refresh frame may carry
  `"engines": { "<model>": "colibri" }` — ONLY the non-default entries.
  Models absent from the map are served by the node's primary upstream
  exactly as before. Pre-v2.5 gateways ignore the key; pre-v2.5 workers
  never send it.
- Routing happens **on the worker**: jobs whose `model` is tagged go to
  `COLIBRI_BASE`, everything else takes the unchanged primary path. A model
  id served by both upstreams stays on the primary. Colibri is chat-only —
  embeddings jobs for a colibri model fail with a named error instead of a
  forwarded 404.
- `GET /fabric/nodes` exposes each node's `engines` map; `GET /v1/models`
  sets `owned_by: "colibri"` when EVERY online node serving that id serves
  it through colibri (one primary-engine copy makes the pooled answer's
  engine unknowable, so the default label wins).

Scheduling is unchanged — colibri models are matched by name like any
other. The GPU-first preference keeps applying; a colibri-only model is
typically advertised by nodes that all run colibri, so the preference
simply picks the least-loaded of them. Expect colibri answers to trade
latency for size: the point is that a 744B-class model is REACHABLE on
community hardware at all.

## Invitations — the provider→consumer connection contract

A provider can address a **specific** consumer instead of serving the
anonymous pool: it mints a **UUIDv4 invitation ID** and shares it (raw, or as
the returned `invite_url`). The invitation is the contract of that peer GPU
relationship — revocable by the provider at any time, no accounts involved on
either side.

Provider surface (auth: the node's own registration token as bearer):
- `POST   /fabric/invitations {label?}` → `{invitation_id, invite_url, …}`
- `GET    /fabric/invitations` — all minted invitations incl. revoked
- `DELETE /fabric/invitations/{id}` — revoke (kept listed, polls as revoked)
- `PUT    /fabric/invitations/{id} {label?, paused?}` — idempotently
  **re-assert** a mirrored invitation (v2.3). Workers keep a local mirror of
  their minted invitations; after connecting to a gateway that lists *zero*
  invitations for this node (including revoked — i.e. the registry was
  lost), the worker re-asserts each mirrored id so consumers' stored
  invitation UUIDs keep working. Ids are never reassigned across nodes
  (409), and a revoked invitation stays revoked.

Consumer surface:
- Send `X-KMPLIFY-Invitation: <uuid>` on `/v1/chat/completions` /
  `/v1/embeddings` to **pin** the job to the inviting node. The EU filter is
  not applied on pinned jobs — the consumer chose this provider deliberately.
- `GET /v1/invitations/{id}` → `{valid, revoked, provider_online, models,
  country}` — the poll target for "wait until the peer returns".

Pinned-job failures are **structured** (`detail` is an object with a `code`),
because client chat UIs render an interactive recovery dialog from them:
- `403 {code: "invitation_invalid"}` — unknown or revoked contract
- `503 {code: "peer_provider_offline"}` — provider not connected right now
- `503 {code: "peer_model_unavailable", models: […]}` — online, wrong model

## Protocol v2 — serverless container workloads

RunPod-serverless model on community peers: a consumer picks a **template**
(ComfyUI, vLLM, ...), the fabric schedules it onto a capable node, the node
runs the container on its GPU, and the consumer reaches the app through the
gateway — relayed over the node's existing outbound WebSocket, so providers
still never open a port.

Trust model (non-negotiable for community peers):
- **Curated templates only.** Consumers pick a template id and can never
  supply an image; the catalog (`GET /v1/templates`) is the gateway's.
- **Node-side image pinning (v2.6).** The node holds its own
  `template → image repository` table and refuses a `workload_start` whose
  image does not match, before pulling anything. The template id was always
  re-validated against the provider's opt-in list, but the id and the image
  arrive in the same frame, so until v2.6 enabling `ollama` meant accepting
  whatever image the gateway attached to that name. The pin is a repository,
  not a full reference: tags and digests may move (the ComfyUI template was
  repinned cu124 → cu126 in flight) but the publisher cannot change. Template
  ids the node has no pin for are refused rather than waved through.
  Operators running their own gateway add pins explicitly with
  `KMPLIFY_FABRIC_EXTRA_IMAGE_PINS="template=repo,..."`.
  Gateways need no change: the field is still sent and is still authoritative
  for *which* tag, just no longer for *whose* image.
- Containers run with no host mounts, `--cap-drop ALL`,
  `--security-opt no-new-privileges`, private 127.0.0.1-only port binding,
  memory/PID caps, and are destroyed at session end. GPU passthrough
  (`--gpus all`) is the only privilege.
- CUDA templates require an NVIDIA node (Linux/Windows-WSL2 with
  nvidia-container-toolkit). macOS nodes cannot host container workloads at
  all (no GPU passthrough into containers) — they stay inference providers.

### Capability advertising (hello v2, backward compatible)

The worker's `hello` may add:
`"workloads": { "enabled": true, "cuda": true, "templates": ["vllm-openai", ...] }`
Absent = inference-only node (v1 behavior, unchanged).

### Session lifecycle

- Consumer: `POST /v1/workloads {"template": "comfyui"}` →
  `{ "session_id", "url": "<absolute>/w/<session_id>/", "node": "<prefix>" }`
  (503 if no capable node). `GET /v1/workloads/{id}` → state; `DELETE` →
  teardown. The session id is the capability token — treat the URL as
  secret. The `url` is absolute (v2.1): built from `FABRIC_PUBLIC_URL` when
  set, else the request's own base URL — a relative path rendered 404s
  against whatever origin the consumer UI happens to be served from.
- Gateway → node: `{ "type": "workload_start", "session", "template",
  "image", "port", "env": {}, "cuda": bool, "mem_gb"?, "volume"? }`,
  `{ "type": "workload_stop", "session" }`. `mem_gb` caps container memory
  (node clamps 1–64, default 8). `volume` is a NAMED volume mount
  `kmplify-fabric-*:<abs path>`; nodes refuse any other shape so a host
  path can never reach `docker run -v`. Both ignored by pre-v2.1 nodes.
  v2.2 adds `env` (template-defined container env vars — the field was
  always sent, empty, so old gateways and nodes interoperate unchanged)
  and `args`: container CMD arguments a node appends AFTER the image name,
  so they reach the container's entrypoint and can never become host-side
  `docker run` flags. First user: the `vllm-openai-lmcache` template's
  `--kv-transfer-config` (docs/LMCACHE.md). A pre-v2.2 node ignores
  `args`, and that template's entrypoint then exits instead of serving —
  a visible failure, never a silently un-cached session.
- Node → gateway: `{ "type": "workload_status", "session",
  "state": "pulling|starting|running|stopped|error", "message"?,
  "progress"? }`. `progress` (0–100, v2.1) is image-pull progress derived
  from layer completion; the gateway exposes it on `GET /v1/workloads/{id}`
  while state is pulling/starting.
- CUDA sessions (v2.1): after starting the container the node verifies the
  GPU is visible INSIDE it (`docker exec … nvidia-smi -L`) and fails the
  session rather than silently serving CPU inference at minutes per answer.
- Node drop → all its sessions are marked `error` and cleaned up locally
  (`docker rm -f`) when the node comes back or exits.

### HTTP relay

`ANY /w/{session}/{path}` on the gateway is forwarded as
`{ "type": "http", "session", "req_id", "method", "path", "query",
   "headers", "body_b64" }`; the node answers
`{ "type": "http_resp", "req_id", "status", "headers", "body_b64" }` after
proxying to the container's private port.

### Node telemetry on `pong` (v2.2)

A node's `pong` may carry live hardware state, refreshed every ping (~20s):

```json
{ "type": "pong",
  "gpu_used_mb": 9826,
  "loaded_models": [
    {"name": "qwen2.5:7b-instruct", "size_vram_mb": 6683, "expires_in_s": 1450}
  ] }
```

`gpu_used_mb` is `nvidia-smi memory.used`. The gateway computes headroom as
`total - max(session reservations, gpu_used_mb)`: reservations know about
sessions before their models load, telemetry knows about everything else on
the machine that reservations cannot see. Reporting reservations alone made
a card with 8 GB parked look completely free.

`loaded_models` mirrors the host Ollama's `/api/ps`, so both the provider's
own UI and consumers can see WHY a peer's VRAM is spoken for — and that it
will free itself when the keep-alive expires. Both fields are optional;
pre-v2.2 workers simply omit them and the gateway falls back to
reservation-only accounting.

Also v2.2: the gateway measures ping->pong round-trip per node and schedules
**nearest-first** (all session traffic relays through the gateway, so
gateway<->node RTT is the honest latency proxy for any consumer), and a node
that declares no `country` gets one derived from its connecting IP — an
undeclared provider used to be invisible to every EU-only consumer, even one
on the same LAN.

### Reasoning control (`think`, v2.1)

Consumers may send `think: true|false` in a chat body. The gateway is
`extra: allow` and forwards it verbatim, but that alone does nothing:
Ollama's OpenAI-compatible endpoint accepts the field and **ignores** it —
measured against qwen3, a reasoning model still burns the whole budget on
the hidden channel and returns `content: ""` with `finish_reason: length`.

A v2.1 node therefore routes any chat job carrying `think` to Ollama's
**native** `/api/chat`, the only endpoint that honors it, translating the
request (`max_tokens` -> `options.num_predict`; `temperature`/`top_p`/
`seed`/`stop` -> `options`) and translating each NDJSON line back to the
OpenAI chunk shape. `message.thinking` maps to `delta.reasoning`, matching
Ollama's own OpenAI surface — never folded into `content`, never dropped.

Omitting `think` keeps the plain OpenAI-compat path, so non-reasoning
models and pre-v2.1 nodes are unaffected.

Streamed responses (v2.1): when the container answers with
`text/event-stream` (SSE — OpenAI-compatible chat) or
`application/x-ndjson` (Ollama's native API), the node forwards it
incrementally instead of buffering the whole generation:
`{ "type": "http_resp_start", "req_id", "status", "headers" }`, then any
number of `{ "type": "http_resp_chunk", "req_id", "body_b64" }`, then
`{ "type": "http_resp_end", "req_id" }`. The gateway relays them as a
chunked HTTP response with a 300s inter-chunk idle timeout. Non-streaming
responses keep the single buffered `http_resp` frame.

Relayed WebSockets (v2.2): a workload's own socket is now carried too, which
ComfyUI needs to work at all — it fetches its page over HTTP and then drives
everything else on `/ws`: execution progress, queue state, binary preview
frames, and the `executed` event delivering the finished image. Over an
HTTP-only relay the page loaded and then sat inert, indistinguishable from a
wedged GPU.

The consumer opens `ws://<gateway>/w/<session_id>/<path>` — the same path as
the HTTP relay, since an Upgrade request routes to the WebSocket handler and
everything else to the HTTP one. Frames:

| Direction | Frame |
| --- | --- |
| gateway -> node | `{ "type": "ws_open", "session", "ws_id", "path", "query" }` |
| gateway -> node | `{ "type": "ws_send", "ws_id", "data_b64", "binary" }` |
| gateway -> node | `{ "type": "ws_close", "ws_id" }` |
| node -> gateway | `{ "type": "ws_opened", "ws_id" }` |
| node -> gateway | `{ "type": "ws_recv", "ws_id", "data_b64", "binary" }` |
| node -> gateway | `{ "type": "ws_closed", "ws_id" }` |
| node -> gateway | `{ "type": "ws_error", "ws_id", "message" }` |

`binary` is carried explicitly rather than inferred: ComfyUI sends previews
as binary frames, and delivering them as text corrupts every image. The node
dials the container and reports `ws_opened` BEFORE any traffic is piped, so a
container listening on HTTP but not upgrading fails with a clean close rather
than a socket that accepts and stays mute (30s open timeout). A pre-v2.2 node
simply ignores `ws_open`, so the consumer's socket times out and closes — an
older provider degrades to today's behaviour instead of breaking.

Still on the optimization path: an optional P2P data channel (WebRTC/QUIC)
for heavy image/video traffic.
