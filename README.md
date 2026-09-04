# kmplify-node

The provider agent for the KMPLIFY Compute Fabric. It lends a machine's GPU, CPU
and locally-served models to a fabric, over a single outbound connection.

This is the half of the fabric that runs on **your** machine, and it is open
source for exactly that reason. Deciding to lend a stranger your GPU is not a
decision anyone should make about a binary they cannot read.

```
        your machine                        someone else's
  ┌───────────────────────┐            ┌──────────────────────┐
  │  kmplify-node         │  outbound  │  gateway             │
  │  (this repository)    │═══════════▶│  scheduler, registry │
  │                       │  WebSocket │  billing, catalog    │
  │  local model server   │            │                      │
  │  optional containers  │            │  (not open source)   │
  └───────────────────────┘            └──────────────────────┘
```

## What it does

- **Serves inference** from a local OpenAI-compatible endpoint (Ollama, vLLM,
  LiteLLM, TGI, llama.cpp) to consumers on the fabric. Prompts and responses
  pass through your machine and the model runs on your hardware.
- **Runs on the GPU you have.** NVIDIA (CUDA), AMD (ROCm), Intel (oneAPI) and
  Apple Silicon (Metal) are detected and reported distinctly, with the right
  container flags per vendor. See the matrix below.
- **Advertises honestly**: real GPU model and VRAM, physical cores rather than
  threads, cgroup limits rather than the host's numbers when containerised.
- **Optionally hosts container sessions** (vLLM, ComfyUI, Ollama, speech
  to text and text to speech) on your GPU.
  Off by default. This is opt-in per template, and never inferred from the fact
  that you have the hardware.
- **Enforces your ceilings**: CPU, VRAM, RAM and disk caps you set are applied
  on this side, not requested politely from the other.
- **Routes across your own network, optionally.** The kmplify-nodes on one
  LAN find each other, pair with a PIN, and one endpoint on any of them
  routes each request to whichever machine can serve it best. A desktop
  window (`kmplify-node gui`) or two extra terminal screens (`tui --router`)
  show the whole cluster. Adapted from NVIDIA's Personal AI Router; see
  [the LAN router](#the-lan-router-and-the-desktop-window) below.
- **Optionally runs signed Wasm functions** in an in-process WASI sandbox
  (stdin/stdout only, no files, no network), only for a catalog key you chose
  to trust, and **optionally lends vector storage** for replicated RAG
  indexes whose payloads are opaque to you. Both off by default and both
  switchable without a restart; protocol v3.0 in [PROTOCOL.md](PROTOCOL.md).

  ```bash
  kmplify-node set functions=true functions-pubkey=$(curl -s https://fabric.kmplify.io/v1/functions | jq -r .pubkey)
  ```

  The modules the public fabric asks nodes to run are in
  [functions/](functions/) — source, build script and hashes — because a
  signature says where bytes came from, not what they do.

### Hardware support

| Platform | Accelerator | Serves inference | Hosts container sessions |
|---|---|---|---|
| Linux | NVIDIA / CUDA | yes | yes (`--gpus all`) |
| Linux | AMD / ROCm | yes | yes (`/dev/kfd` + `/dev/dri`) |
| Linux | Intel / oneAPI | yes | yes (`/dev/dri`) |
| Windows | NVIDIA / CUDA | yes | yes, native Linux containers only |
| Windows | AMD / ROCm | yes | HIP SDK dependent |
| macOS | Apple Silicon / Metal | yes | **no** |
| any | CPU only | yes (small models) | yes (CPU templates) |

Two honest caveats. **macOS cannot host sessions at all**: the Docker daemon
runs in a VM with no GPU passthrough, so a Mac lends inference through its
local model server and the node refuses session work rather than accepting it
and running on CPU at minutes per answer. And **vLLM templates need native
Linux**, not WSL2, because vLLM's V1 engine requires UVA.

Detection is automatic. `KMPLIFY_GPU_BACKEND=cuda|rocm|oneapi|metal|cpu`
overrides it, and `kmplify-node check` prints every accelerator it found plus
which one goes on the wire.

There are deliberately **two** questions being asked, and they can disagree:

- **Is a GPU usable, and how big?** The vendor tool is executed and its VRAM
  parsed. This is what the node advertises, because advertising capacity to a
  fabric promises that work can actually land on it.
- **Is a driver stack installed?** Filesystem lookups only, no subprocess.
  This is what decides whether local inference is worth attempting at all.

A machine with `nvidia-smi` present but a broken driver answers CUDA to the
second and CPU to the first, and both answers are right. `check` says so
explicitly when they differ, so "I installed ROCm and it still says cpu" has a
visible cause rather than being a mystery: the stack is there, its tool did
not answer, and the fabric only advertises what it can size and serve.

CUDA and Metal are verified on real hardware. The ROCm and oneAPI paths are
written against the documented output of `rocm-smi`, `amd-smi` and `xpu-smi`
and unit-tested on captured samples, but have not yet run on an AMD or Intel
GPU. If you have one, `kmplify-node check` output is the most useful bug
report you can file.

## What it does not do

The fabric worker never listens on a port. It opens one outbound WebSocket
and everything, including the HTTP relay to a hosted session, travels back
over it. Joining a fabric does not expose your machine to the internet, and
you do not need to forward a port, open a firewall or own a domain. (The
LAN router is the one opt-in exception, and it listens on the local network
only; what it opens and for whom is spelled out in its own section.)

It has no account. A node registers anonymously and gets an opaque id and
token; there is no email, no PII, and nothing tying the machine to a person
unless you separately link it for payouts.

It holds no wallet. No key material, no address, no chain client, no token —
being paid is an optional companion program that reads what this node
publishes and settles against the fabric's own signed receipts. See
[docs/REWARDS.md](docs/REWARDS.md) for the boundary and
[`kmplify-node rewards`](#rewards--what-it-delivered-and-optionally-what-that-earns)
for what an operator sees.

## Install

One line, on Linux (x86_64/aarch64) and macOS. It downloads the release
binary for this machine, verifies it against the release's SHA256SUMS, and
ends with the `check` preflight:

```bash
curl -fsSL https://raw.githubusercontent.com/kmplify/kmplify-node/main/scripts/install.sh | sh
```

For a 24/7 server or VPS, let it also set up the systemd service (creates
the `kmplify` user, installs the unit and `/etc/kmplify-node.env`):

```bash
curl -fsSL https://raw.githubusercontent.com/kmplify/kmplify-node/main/scripts/install.sh | sudo sh -s -- --service
```

As a container (inference-only; container sessions need the binary on the
host, where it can drive Docker and see the GPU):

```bash
docker run -d --name kmplify-node --network host -v kmplify-node-data:/data ghcr.io/kmplify/kmplify-node
```

Or from source (add `--features gui` for the desktop window):

```bash
cargo install --git https://github.com/kmplify/kmplify-node
```

On Windows, where `install.sh` does not run, there is an installer
(`kmplify-node-<version>-setup.exe`: the binary, a Start menu entry that
opens the window, optional PATH and sign-in autostart), or the bare
`kmplify-node-x86_64-pc-windows-msvc.exe`, or the source build above with
Rust 1.95 or newer.

Desktop packages for the window, from the same release:

| Platform | Package | What it contains |
|---|---|---|
| Debian, Ubuntu and derivatives | `kmplify-node_<version>_amd64.deb` / `_arm64.deb` | the binary with the window, a desktop entry and icon, the systemd unit (installed, not enabled) |
| macOS | `kmplify-node-<version>-<arch>.dmg` | `KMPLIFY Node.app`; the binary inside it is the full CLI, so `ln -s "/Applications/KMPLIFY Node.app/Contents/MacOS/kmplify-node" /usr/local/bin/` gives you the commands too |
| Windows | `kmplify-node-<version>-setup.exe` | as above |

The macOS bundle is signed ad hoc and not notarised: the first open is a
right-click, Open. The bare macOS and Windows binaries are built with the
window as well; the static Linux binaries are headless, which is what a
server wants, and the `.deb` is the one with the window.

Prebuilt binaries, their SHA256SUMS, the packages, the systemd unit and the
env template are attached to every
[GitHub release](https://github.com/kmplify/kmplify-node/releases).

## Run

First time? One command walks the whole setup with you:

```bash
kmplify-node init
```

It looks at the machine, scans localhost for the inference engine you already
run (Ollama, llama.cpp, vLLM, LM Studio, LiteLLM, Jan, colibri), asks the
sharing questions in plain words, fetches the fabric's function key if you
opt into that lane, saves everything, preflights it and offers to start.
Nothing is shared until you confirm the summary, and Ctrl-C abandons all of
it. The answers land in the same `settings.json` that `kmplify-node set` and
the dashboard write, so nothing about the wizard is special — it is just the
guided way in. The full annotated walkthrough, captured from a real run, is
in [docs/ONBOARDING.md](docs/ONBOARDING.md).

Already configured? The short loop is:

```bash
kmplify-node check
```

```bash
kmplify-node
```

Then watch and steer it from a terminal:

```bash
kmplify-node tui
```

Or, on a machine with a desktop, open the window instead, which shows the
same node plus every other kmplify-node on your network:

```bash
kmplify-node gui
```

The window and the dashboard are two views of one node, and you can switch
between them at any time: each one **attaches** to the node (and to the LAN
router) already running here, and only starts them itself when nothing is
running. So `kmplify-node run --router` as a service, `gui` on the desk and
`tui --router` over SSH all show and steer the same thing, and closing any
of the views stops nothing that was running before it opened. Details in
[the router section](#the-lan-router-and-the-desktop-window).

### Your engine, found rather than typed

KMPLIFY ships no model runner of its own; the node lends whatever
OpenAI-compatible engine you already run. `engines` shows what is listening
and which one the node uses:

```bash
kmplify-node engines
```

```
  Ollama       http://127.0.0.1:11434   17 (qwen3:0.6b, bge-m3:567m, …) <- active
  LM Studio    http://127.0.0.1:1234    28 (qwen/qwen3.8-27b, …)

suggestion: mlx fits this machine (Apple Silicon), it is not running yet — kmplify-node set engine=mlx
```

The suggestion is the hardware speaking — MLX on Apple Silicon, llama.cpp
with CUDA/ROCm offloading where a card is usable — and `init` highlights the
same row in its engine menu. It never applies itself: switching stays your
one deliberate command.

Switching is one durable setting, by name or by URL, applied on the
reconnect it triggers:

```bash
kmplify-node set engine=lmstudio
```

```bash
kmplify-node set engine=http://10.0.0.7:8000
```

(The underlying variable keeps its historic name, `OLLAMA_BASE`; the engine
behind it is anything that speaks the OpenAI API.)

Everything below is the detail behind those commands.

## The CLI

One binary, every command. Everything an operator does to a node — preflight
it, run it, watch it, change what it lends, decide who may use it — is here,
so a machine with no desktop is no harder to run than one with a window.

| Command | What it does |
|---|---|
| `kmplify-node init` | First-run wizard: find your engine, answer six questions, preflight, start. |
| `kmplify-node` / `run` | Join the fabric and serve. Logs to stdout, stops cleanly on SIGTERM. `--router` also runs the LAN router. |
| `kmplify-node engines` | Scan localhost for inference engines; say which one is active. `--json`. |
| `kmplify-node tui` | Terminal dashboard: watch **and control** the node. `--router` adds the network and cluster screens. |
| `kmplify-node gui` | The desktop window: this node, the LAN's other nodes, routing, pairing, engines. Builds with `--features gui`. |
| `kmplify-node check` | Preflight this host. Connects to nothing. `--json`, `--timeout SECS`. |
| `kmplify-node status` | Is it serving right now, and how hard is the machine working. `--json`. |
| `kmplify-node set` | Change what this machine lends, durably and without a restart. |
| `kmplify-node peers` | Who may use it — list, approve, deny, block, invite, revoke. `--json`. |
| `kmplify-node rewards` | What this node delivered, and what an optional payout companion says. |
| `kmplify-node id` | Print this install's node id, the handle consumers pin and invite. |
| `kmplify-node version` · `help` | Version and build stamp; the full flag list. |

**One configuration surface.** Settings are environment variables, so a
service manager, a container and a shell all configure it the same way — and
every one also has a flag that *sets that variable* rather than living beside
it. `--gateway https://…` is `PROVIDER_GATEWAY_URL=https://…`, no more and no
less, which is what lets `check` report exactly what `run` would use.

**Exit codes**, so a provisioning script can branch: `0` ready or a clean
stop, `1` this host cannot serve as configured (or nothing is running), `2`
the command line or the configuration is wrong. A value that cannot be read —
a ceiling that is not a number, a country that is not alpha-2 — stops the node
at startup naming the variable, rather than falling back to a default nobody
chose.

### `check` — will this host actually serve?

Run it first. It connects to nothing: it resolves the configuration, probes
Docker, the vendor GPU tools, the gateway and your model endpoint, and tells
you what the fabric would see.

```bash
kmplify-node check
```

```
kmplify-node 0.3.0+c8ba618

configuration
  gateway    : https://fabric.kmplify.io
  creds      : ~/.config/kmplify-node/fabric_node.json
  ollama     : http://127.0.0.1:11434
  inference  : ON
  sessions   : OFF (set PROVIDER_WORKLOADS to opt in)
  ceilings   : cpus=None vram_mb=None ram_mb=None disk_gb=None

accelerator
  metal      : Apple M2 Max (49152 MB) <- advertised
  advertised : metal

probes
  gateway    : reachable (http 405)
  docker     : 29.6.2
  models     : 17 (qwen3:0.6b, bge-m3:567m, gemma4:12b-mlx, …)

WARNING: PROVIDER_COUNTRY is undeclared, so the gateway records XX and
         consumers filtering for EU residency will not see this node

ready.
```

A node that lists no models will connect, count as online, and have every job
refused by the scheduler — so `check` fails loudly on that rather than letting
you deploy something that looks healthy and serves nothing. Probes run
concurrently with a per-probe timeout, so a wedged Docker socket cannot hang
it. `--json` gives the same verdict, warnings and errors in a form a script
can read.

Flags are handy for trying a setting before writing it into a unit file:

```bash
kmplify-node check --workloads vllm-openai --max-vram-mb 16000
```

### `run` — join the fabric and serve

```bash
kmplify-node
```

Logs to stdout (journald adds the timestamps). On SIGTERM — what `systemctl
stop` sends — the worker tears down every hosted session container before
exiting, so a stopped node leaves nothing of other people's running on your
GPU. If the worker ever dies unexpectedly the process exits non-zero, so a
service manager restarts it instead of leaving a healthy-looking process that
lends nothing.

For a 24/7 box see [docs/HEADLESS-NODE.md](docs/HEADLESS-NODE.md) and the
systemd unit in [packaging/](packaging/).

### `status` — what is it doing right now?

```bash
kmplify-node status
```

```
ONLINE pid 30736 up 3h 12m
  node     : 8f2c1a5b90de4a17b3c9d0e5f6a7b8c9
  gateway  : https://fabric.kmplify.io
  models   : qwen3:0.6b, bge-m3:567m, gemma4:12b-mlx, …
  load     : cpu 38%  gpu 70%  ram 41/64 GB
  jobs     : 2 active, 914 finished, 3 errors, avg 812 ms
  sessions : 1
```

Exits `1` when no node is running here, so `kmplify-node status >/dev/null ||
alert` is a monitor. `--json` adds every field the dashboard draws — link
state, per-core load, VRAM, ceilings, hosted sessions, the log tail — which is
the machine-readable half of the monitoring story below.

### `set` — change what you lend, without a restart

The dashboard's sharing screen for a shell:

```bash
kmplify-node set max-cpus=6 share-cpu=true workloads=vllm-openai,comfyui
```

```bash
kmplify-node set --list          # what is stored here, and what it overrides
kmplify-node set --clear max-cpus   # back to whatever the unit file says
```

| Setting | Overrides | Means |
|---|---|---|
| `share-inference` | `PROVIDER_SHARE_INFERENCE` | serve chat and embedding jobs |
| `share-cpu` | `PROVIDER_SHARE_CPU` | lend spare CPU threads and RAM |
| `workloads` | `PROVIDER_WORKLOADS` | container templates to host; empty = sessions off |
| `approval-mode` | `PROVIDER_APPROVAL_MODE` | `auto` or `manual` admission |
| `country` | `PROVIDER_COUNTRY` | ISO alpha-2, for consumers who want EU capacity |
| `colibri` · `colibri-key` | `COLIBRI_BASE` · `COLIBRI_API_KEY` | second upstream for frontier MoE models |
| `max-cpus` · `max-vram-mb` · `max-ram-mb` · `max-disk-gb` | the matching `PROVIDER_MAX_*` | ceilings peer sessions never exceed |
| `functions` · `functions-pubkey` | `PROVIDER_FUNCTIONS` · `PROVIDER_FUNCTIONS_PUBKEY` | host signed Wasm functions, and the catalog key to trust |
| `share-vectors` · `max-vector-mb` | `PROVIDER_SHARE_VECTORS` · `PROVIDER_MAX_VECTOR_MB` | hold peers' vector collections, and how much |
| `rewards` | `PROVIDER_REWARDS` | may the node ask an installed payout companion |

Writes `settings.json` in the node directory (mode 0600 — it can hold the
colibri key) and nudges the running node, which re-advertises within a second;
a node that is offline picks the change up on its next connection. **A stored
choice overrides the environment** — the same contract the desktop app has —
so a ceiling does not spring back on the next restart. Nothing about that is
silent: `check` prints every value currently overriding the environment,
`set --list` shows them, and clearing one restores the unit file's value with
no restart.

### `peers` — who may use this machine

`auto` (the default) lets any consumer on the fabric use it. `manual` parks
unknown consumers until you decide: they are told to ask, and they wait.
Invitations connect in either mode — minting one *is* the approval.

```bash
kmplify-node set approval-mode=manual
kmplify-node peers
```

```
admission : manual

waiting for a decision (1)
  anon-1a2b3c4d            waiting 2m       asked for llama3
      kmplify-node peers approve anon-1a2b3c4d

consumers seen recently (2)
  node-9abc…               active  via grid selection   last seen 3s   approved
  anon-deadbeef            idle    via pool             last seen 9m   blocked

invitations (1)
  7f9b2c9e-4a1d-4e5f-9c3a-2b8d1e6f0a47  Anna's phone         in use
```

| Verb | What it does |
|---|---|
| `peers approve <consumer>` | admit them — a standing rule, so it holds |
| `peers deny <consumer>` | refuse quietly while manual admission is on |
| `peers block <consumer>` | refuse in **every** mode |
| `peers clear <consumer>` | drop the rule; the admission mode decides again |
| `peers invite [label]` | mint an invitation; the id goes to stdout |
| `peers revoke <id>` | end an invitation for good |

It talks to the gateway with the node's stored credential rather than through
the running worker, so approving from cron works whether or not the node is
up. The listing reports the mode the **gateway** believes, falling back to the
configured one only while the node is offline — the two differ exactly while a
change has not been re-advertised, and the gateway's answer is the one that
decides who gets in.

Invitations are meant to be scripted:

```bash
INVITE=$(kmplify-node peers invite "Anna's phone")
```

### `rewards` — what it delivered, and (optionally) what that earns

The node lends hardware. It holds no wallet, signs no transaction and knows no
token — being paid is a **separate program** an operator installs on purpose.
This command shows the node's own half either way:

```bash
kmplify-node rewards
```

```
this node
  node id  : 44802ebc90e24de18825b90af320edfe
  gateway  : https://fabric.kmplify.io

delivered since this node started (6h 12m)
  jobs     : 914 answered in 742.1 s of compute
  sessions : 3 hosted, 14h 24m of machine time
  (the node's own count — the fabric's signed receipts are what settle)

rewards companion
  off. Rewards are optional and this node needs nothing to serve.
```

Install a companion and switch it on, and the same command — and the
dashboard's home screen — shows what it reports:

```bash
kmplify-node set rewards=on
```

```
rewards companion
  /usr/local/bin/chaingence-plugin
  TESTNET  12.40 tEURC pending  ·  0.00 tEURC paid  ·  evm:base-sepolia
```

Two deliberate acts are required, because running another program is not
something a node should do because a binary happened to be on `PATH`. The
node publishes `identity.json` — its public node id, never its credential —
and the delivered-work counters above; a companion reads those and settles
against the fabric's signed receipts. Nothing about serving depends on it, and
a companion that is missing, slow or unhappy costs one line of output and
nothing else.

The full contract, including what the node will never do, is in
[docs/REWARDS.md](docs/REWARDS.md). Payout rails are testnet-only today: a
balance that cannot be spent is labelled `TESTNET` everywhere it appears.

## The LAN router and the desktop window

A household or a small office rarely has one machine with a GPU; it has a
gaming PC, a Mac, a NAS with a spare card, a laptop. The router turns the
kmplify-nodes on one local network into a **personal inference cluster**:
every machine finds the others, an application on any of them talks to one
endpoint, and each request goes to whichever machine can serve that model
best. Prompts and answers travel between machines you own and never leave
the network. The design is adapted from NVIDIA's Personal AI Router
(Apache-2.0, see [NOTICE](NOTICE)), reimplemented in Rust inside this one
binary for Linux, macOS and Windows; the full design, the wire formats and
what was verified are in [docs/ROUTER.md](docs/ROUTER.md).

It is opt-in per machine and off by default, because it is the one part of
this program that listens on the network.

### Three ways to run it, one router

```bash
kmplify-node gui
```

```bash
kmplify-node tui --router
```

```bash
kmplify-node run --router
```

The router runs in exactly one process on a machine. The first of these to
start it owns it; every later one **attaches**: it draws the state that
process publishes (`router.json` in the node directory, once a second) and
leaves its orders as files (`control/router/`), the same two mechanisms the
fabric node already uses for `status.json` and `control/`. So the window can
be closed and the terminal dashboard opened, or the other way round, and
the cluster, the proxies and the peer polls carry on. The status bar (window)
and the router panel (dashboard) say which it is: "router in this window" or
"attached to the router running here". `--standalone` insists on running
both node and router in this process.

On Windows and macOS the window has a tray icon: closing the window while it
hosts the node or the router hides it there, and the tray menu opens it
again or quits for real. The settings screen has the "open when I sign in"
switch (a per-user Run entry, LaunchAgent or XDG autostart file, and nothing
system-wide). On Linux the window closes like any window; a router that
should outlive it is `kmplify-node run --router` as a service, with the
window or the dashboard attaching to it.

### What you see

**Overview**: a card per node with its GPU, engines and model counts, a
radial gauge and a minute of history for GPU load, VRAM, CPU and RAM, and a
jobs column on the left with a line from each running job to the node
serving it. **Settings**: the cluster card (pair, invite, remove, leave), the
network card (LAN ingress, nodes added by address, the router's log), this
window's autostart, and the same sharing switches `kmplify-node set` writes.
**Endpoints**: what to paste into an application. **Chat**: a message typed
there is routed exactly like an application's request. Each node card's
engine panel installs, starts, stops and pulls models for Ollama and LM
Studio, on this machine or on a paired one.

The terminal dashboard gets the same as two screens: `8` network (every node
with state, address, GPU, engines, load and pending work; the routed jobs;
the endpoints) and `9` cluster (fingerprint, members, `i` invite, `o` join,
`d` remove, `L` leave).

### Discovery, pairing, trust

Nodes announce one `_kmplify-node._tcp` record on the local link (multicast
DNS, with the router's own responder because Windows ships none) carrying
capacity, never content: id, name, GPU, VRAM, cores, RAM, engines, version,
cluster id. A node the network cannot see (another subnet, a VPN) is added
by address.

Seeing a node is not trusting it. Every node has a self-signed certificate;
**pairing** is a six-digit PIN shown on one machine and typed on the other,
authenticating a SPAKE2 key exchange so an active attacker gets one online
guess per attempt and nothing to brute-force offline. Each side pins the
other's certificate fingerprint in `cluster.json`, and from then on every
request between them travels over **mutual TLS** pinned to those
fingerprints. There is no certificate authority, no account and no cloud
step in any of this.

| Port | Listener | Who may use it |
|---|---|---|
| `14418` | node-info: hardware, meters, engines with model names, pending work, recent jobs; pairing; engine control | plain HTTP from the subnet **only while the node is in no cluster**; afterwards loopback and paired nodes over mutual TLS. Pairing itself is plaintext, authenticated by the PIN. |
| `11440` | Ollama-compatible routing proxy (`/api/*`, plus the `/v1/*` Ollama serves) | this machine (routed anywhere in the cluster); paired nodes over mutual TLS while *LAN ingress* is on (served by this node's own engine, never routed on); anything else `403` |
| `11441` | OpenAI-compatible routing proxy (`/v1/*`, every engine) | same |

The router never takes over an engine's own port: Ollama stays on `11434`,
LM Studio on `1234`, and an application opts into the cluster by pointing
at `11440` or `11441`. Switch LAN ingress off and a machine becomes a pure
consumer of the cluster that serves nothing to it.

### Routing

`GET /api/tags` and `GET /v1/models` fan out to every online node and merge
by model name, listing every owner. Any other request names a model; the
candidates are the online paired nodes whose running engine has it, ordered
by pending work plus GPU pressure (smoothed, with hysteresis), and tried in
order. A `404` (stale inventory) or a `5xx` moves to the next owner; a
`400` is returned as is; the response streams back untouched. Each request
is a job card, opened at dispatch and closed when the response body ends,
replicated to every node so every window shows work running anywhere.

### The router and the fabric

The two are independent and combine in one line. The fabric node lends
whatever engine `engine=` points at; point it at the router and the fabric
gets the whole cluster:

```bash
kmplify-node set engine=http://127.0.0.1:11440
```

The node re-advertises the union of the cluster's models, and a fabric job
that lands here is routed to the best machine on the LAN like any local
request, over loopback, so the router's caller gate never sees a fabric
consumer. Nothing on the wire to the gateway changes (the gateway's hello
handler tolerates every field the node sends), and the gateway's own test
suite includes a cross-repository check that this node's image pins and the
gateway's catalog agree. The relevant protocol sections are mirrored in
[PROTOCOL.md](PROTOCOL.md).

### Environment

| Variable | Default | Meaning |
|---|---|---|
| `KMPLIFY_ROUTER_PORTS` | `14418,11440,11441` | node-info, Ollama-compatible proxy, OpenAI-compatible proxy; also what lets two nodes share one machine for testing |
| `KMPLIFY_NODE_NAME` | the hostname | the name this node announces and shows |

Files in the node directory: `router/node.crt.der` and `node.key.der` (this
node's certificate), `router/cluster.json` (cluster id, members with pinned
fingerprints and last known addresses), `router/engines/` (an Ollama the
router installed itself), `router.json` (the published state) and
`control/router/` (orders for it).

## The terminal dashboard

A node usually runs where there is no desktop, which used to mean it was
operated by reading logs. `kmplify-node tui` is the desktop app's provider
screens in a terminal, on Linux, macOS and Windows alike:

```bash
kmplify-node tui
```

It **attaches** to the node already running here (systemd, Docker, launchd),
reading the snapshot that node publishes and sending commands back to it, so
quitting the dashboard leaves the node running. If nothing is running, it
starts a node itself and quitting stops it. `--attach` insists on the first,
`--standalone` on the second.

| Screen | What it shows |
|---|---|
| `1` home | link state, four live meters, what is advertised, jobs, sessions, log |
| `2` sessions | peers' containers running here |
| `3` models · `4` log | what consumers can ask for; the worker's log |
| `5` sharing | what this machine lends, and how much of it |
| `6` peers | who may use it: waiting consumers, active ones, invitations |
| `7` activity | CPU, GPU, VRAM and RAM live, with history and a bar per core |
| `8` network · `9` cluster | with `--router`: the LAN's nodes, routed jobs and endpoints; pairing and members |

| Key | Action |
|---|---|
| `p` | Pause or resume sharing. The connection and hosted sessions stay up; the node advertises nothing until resumed. |
| `c` | Reconnect to the gateway now, without waiting out the backoff. |
| `e` | Evict the selected session — the peer's container is stopped and removed. |
| `x` | Stop the node, tearing down hosted sessions first. |
| `w` | Write a plain-text snapshot of the dashboard, for a bug report. |
| `?` | Every key, including the ones each screen adds. |

The node publishes `status.json` in its node directory (owner-readable only)
and accepts commands as files in `control/` there. That is how a dashboard in
one process drives a node in another **without the node ever listening on a
port** — the property this whole crate is built around. It also means the
dashboard must run as the user that owns that directory; for the systemd
install that is `kmplify`:

```bash
sudo -u kmplify KMPLIFY_NODE_DIR=/var/lib/kmplify-node kmplify-node tui
```

### Monitoring: the activity screen (`7`)

The question a provider actually has is "how much of my machine is gone right
now", and no log line answers it. `top` in another window cannot tell peer
work apart from your own.

```
╭ CPU ───────────────────────────────────╮╭ GPU · cuda ────────────────────────────╮
│ ████████████········  61%  4 of 16 lent ││ ██████████████······  70%  busy        │
│      ▂▃▅▆█▇▅▃▂▁▂▃▅▆█▇▅                  ││    ▁▃▅▆████▇▅▃▁▂▄▆██▇                  │
│ 13th Gen i9-13900K   5 min: avg 38% …   ││ RTX 4090 · 24576 MB  5 min: avg 52% …  │
╰────────────────────────────────────────╯╰────────────────────────────────────────╯
╭ System RAM ────────────────────────────╮╭ VRAM ──────────────────────────────────╮
│ ████████████████████  64%  41 / 64 GB   ││ ███████·············  36%  8 / 24 GB   │
╰────────────────────────────────────────╯╰────────────────────────────────────────╯
╭ cores ──────────────────────────────────────────────────────────────────────────╮
│  0 ███████···  71%    4 ██········  22%    8 █████·····  54%   12 ██········  19%│
│  1 ██████····  64%    5 █·········  11%    9 ███·······  31%   13 █·········  8% │
╰─────────────────────────────────────────────────────────────────────────────────╯
╭ what the fabric is holding ─────────────────────────────────────────────────────╮
│ sessions 1   cores held 4.0   jobs 2   finished 914   fabric disk 38.2 GB       │
╰─────────────────────────────────────────────────────────────────────────────────╯
```

Four measurements, each with a bar, the reading and **five minutes of
history**; a bar per logical CPU, which is what tells a pinned thread apart
from a busy machine; and a line for what peers are holding right now. The home
screen carries the same four meters in miniature with an inline history strip.

Colour carries meaning rather than decoration: each measurement keeps one
colour everywhere it appears (CPU cyan, GPU magenta, VRAM green, RAM blue),
each screen has its own, and a meter tints amber then red as the next request
stops fitting.

GPU load and VRAM come from `nvidia-smi` / `rocm-smi` in a single probe every
few seconds — a node lending its cycles should not spend them on being
watched. **Where a platform will not report a figure, the panel says so**
rather than drawing a zero: macOS exposes GPU load only to privileged tools,
and unified memory has no distinct "VRAM used", so a flat line along the
bottom of a graph would read as an idle card every time.

Everything on this screen is in `status.json` too, so `kmplify-node status
--json` feeds the same numbers to a monitoring system, and an attached
dashboard graphs exactly what the node measured.

### Sharing (`5`)

The desktop app's "Provide this machine's Resources" panel: switches for
inference, container sessions **per template** (built from what this
accelerator can actually host, so a Mac is offered none), CPU/RAM and manual
approval; ceilings for cores, VRAM, RAM and disk; country and colibri
upstream.

```
 what this machine lends
  [x] GPU inference (chat & embeddings)      7 model(s) advertised
  [x] CPU & system RAM                     ● 16 cores, 64 GB
  [ ] Require my approval for new peers      consumers wait until approved

 ceilings — peer sessions never exceed these
  CPU threads             ██████████████░░░░░░  9 / 16 threads ●
  VRAM                    ████████████████████  all of it (24 GB)
```

`space` toggles · `←/→` moves a ceiling (shift for a bigger step) · `enter`
edits a field · `d` hands a row back to the environment · `s` applies. Edits
are a **draft** until applied, because a ceiling is re-advertised by
reconnecting and reconnecting on every arrow key would make the node flap. A
`●` marks a value that is overriding the environment.

### Peers (`6`)

The same admission work as `kmplify-node peers`, live: `a` approve, `n` deny,
`b` block, `u` clear the rule, `i` mint an invitation, `h` hold or resume one,
`v` revoke it. It exists because the sharing screen can turn manual approval
on, and a node in manual mode with no way to approve anybody has quietly
stopped serving.

## Configuration

Environment variables, so a service manager, a container and a shell all
configure it the same way; every one also has a flag that sets it, and the
sharing ones can be changed at runtime from the dashboard or `kmplify-node
set` — which then wins over the environment until cleared. A value
that cannot be read — a ceiling that is not a number, a country that is not
alpha-2, an admission mode that is neither `auto` nor `manual` — stops the
node at startup with a message naming the variable, rather than silently
falling back to a default the operator did not choose.

| Variable | Default | Meaning |
|---|---|---|
| `PROVIDER_GATEWAY_URL` | the public fabric | Gateway to join. Point it at your own. |
| `OLLAMA_BASE` | `http://127.0.0.1:11434` | Any OpenAI-compatible endpoint, despite the name. |
| `PROVIDER_WORKLOADS` | *empty* | Container template ids to host (`vllm-openai`, `vllm-openai-lmcache`, `comfyui`, `comfyui-api`, `ollama`, `ollama-cpu`, `speaches`, `speaches-cpu`). Empty means sessions are off. |
| `PROVIDER_COUNTRY` | *empty* | ISO alpha-2, so consumers can prefer EU capacity. Self-declared, see below. |
| `PROVIDER_SHARE_INFERENCE` | `true` | Serve model jobs at all. |
| `PROVIDER_SHARE_CPU` | `false` | Offer CPU and RAM as lendable capacity. |
| `PROVIDER_MAX_CPUS` | *unset* | Ceiling on cores lent to sessions. |
| `PROVIDER_MAX_VRAM_MB` | *unset* | Ceiling on advertised VRAM. |
| `PROVIDER_MAX_RAM_MB` | *unset* | Ceiling on advertised RAM. |
| `PROVIDER_MAX_DISK_GB` | *unset* | Ceiling on disk sessions may fill. |
| `PROVIDER_APPROVAL_MODE` | `auto` | `manual` holds each session for your approval. |
| `COLIBRI_BASE` | *empty* | Optional second upstream for MoE streaming. |
| `KMPLIFY_NODE_DIR` | `$XDG_CONFIG_HOME/kmplify-node` | Where the node identity is stored. |
| `KMPLIFY_ROUTER_PORTS` · `KMPLIFY_NODE_NAME` | `14418,11440,11441` · hostname | The LAN router's ports and announced name. See [the router section](#the-lan-router-and-the-desktop-window). |
| `KMPLIFY_GPU_BACKEND` | autodetect | Force the accelerator: `cuda`, `rocm`, `oneapi`, `metal`, `cpu`. |
| `KMPLIFY_CUDA` | autodetect | Older CUDA-only override (`1`/`0`). Still honoured. |
| `KMPLIFY_FABRIC_EXTRA_IMAGE_PINS` | *empty* | Extra `template=repository` image pins. See below. |
| `PROVIDER_FUNCTIONS` | `false` | Host signed Wasm functions. The runtime ships in the released binaries; this switch and a catalog key are what turn it on. |
| `PROVIDER_FUNCTIONS_PUBKEY` | *empty* | Hex Ed25519 key of the function catalog to trust. Empty = refuse all. |
| `PROVIDER_MAX_FUNCTION_MB` | `256` | Per-call memory ceiling (hard cap 1024). |
| `PROVIDER_MAX_FUNCTION_MS` | `30000` | Per-call wall-clock ceiling (hard cap 300000). |
| `PROVIDER_REWARDS` | `false` | May the node ask an installed payout companion for a status line. See [docs/REWARDS.md](docs/REWARDS.md). |
| `PROVIDER_SHARE_VECTORS` | `false` | Lend storage for replicated vector collections. |
| `PROVIDER_MAX_VECTOR_MB` | `1024` | Ceiling on stored collections. |

`PROVIDER_COUNTRY` is self-declared and the gateway cannot verify it. It exists
so consumers can *prefer* EU/EEA capacity, which is a data-residency preference
and not an attestation. Declaring a country you are not in defeats the point
for the consumer, not for you.

## Trust model

The gateway schedules. The node decides what its own hardware does. Every rule
below is enforced here, in code you can read, and none of them depend on the
gateway behaving:

- **Sessions are opt-in per template.** No `PROVIDER_WORKLOADS`, no containers.
- **Images are pinned on this side.** Each template maps to an image repository
  in [`src/fabric_worker.rs`](src/fabric_worker.rs), and a `workload_start`
  whose image does not match is refused before anything is pulled. Enabling the
  `ollama` template means `ollama/ollama` and nothing else. Tags and digests
  are free to move, publishers are not, and a template id this build has never
  heard of is refused rather than waved through. Running your own gateway with
  your own catalog means adding pins explicitly with
  `KMPLIFY_FABRIC_EXTRA_IMAGE_PINS="my-template=my-org/my-image"`, which is a
  sentence you should have to write down rather than inherit.
- **No host mounts.** Only `kmplify-fabric-*` named volumes onto absolute
  paths, so `-v /:/host` cannot be smuggled through a template.
- **Containers are boxed**: `--cap-drop ALL`, `--security-opt no-new-privileges`,
  memory and PID caps, and a 127.0.0.1-only port binding. GPU passthrough is
  the only privilege granted.
- **Ceilings are clamped here.** A session's memory cap, CPU share and readiness
  timeout all arrive as requests and are clamped locally, so a gateway cannot
  pin a container on your machine indefinitely or hand it the whole box.
  Memory follows the same rule as cores: your `PROVIDER_MAX_RAM_MB` wins, and
  without one a session never takes more than three quarters of the host.
- **Model downloads are vetted twice.** Only https, only an allowlist of
  model hosts (Hugging Face, Civitai, GitHub), only into the template's model
  volume. The URL is handed to the downloader as an argument, never
  interpolated into a shell command.
- **The relay stays on loopback.** Relayed requests and sockets can only
  reach the session container on 127.0.0.1; a path that tried to name
  another host is refused.
- **Stopping means stopping.** SIGTERM tears down every hosted session before
  the process exits, and a container that fails to die is reported rather than
  optimistically declared gone.

If you find a gap in any of this, please read [SECURITY.md](SECURITY.md) before
opening a public issue.

## Running your own fabric

Nothing here is specific to the public KMPLIFY gateway. Point
`PROVIDER_GATEWAY_URL` at your own, implement the gateway side of
[PROTOCOL.md](PROTOCOL.md), and this node will serve it. That is a supported
use, not a tolerated one.

## Relationship to the rest of KMPLIFY

KMPLIFY is a private AI stack: local models, retrieval over your own documents,
and a fabric that lets machines lend each other capacity. The products around
this node are commercial and closed.

The line is deliberate and it is not going to move: **what runs on your machine
is open, what runs on ours is not.** The scheduler, registry, billing and
marketplace are the business. The agent on your hardware is something you are
entitled to audit, and that is worth more to us as source than as a secret.

The same worker powers the KMPLIFY desktop app's provider mode. This repository
is the headless build of it.

## Contributing

Yes, please, especially hardware coverage and the trust rules. See
[CONTRIBUTING.md](CONTRIBUTING.md). Contributions require a
[CLA](CLA.md).

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The licence covers the code, not the name: KMPLIFY and kmplify-node are
trademarks, and modified versions need their own name. What that means in
practice, including everything you may do without asking, is spelled out in
[TRADEMARKS.md](TRADEMARKS.md).
