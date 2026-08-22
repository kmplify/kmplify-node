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
- **Optionally runs signed Wasm functions** in an in-process WASI sandbox
  (stdin/stdout only, no files, no network), only for a catalog key you chose
  to trust, and **optionally lends vector storage** for replicated RAG
  indexes whose payloads are opaque to you. Both off by default; protocol
  v3.0 in [PROTOCOL.md](PROTOCOL.md).

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

It never listens on a port. It opens one outbound WebSocket and everything,
including the HTTP relay to a hosted session, travels back over it. Joining a
fabric does not expose your machine to the internet, and you do not need to
forward a port, open a firewall or own a domain.

It has no account. A node registers anonymously and gets an opaque id and
token; there is no email, no PII, and nothing tying the machine to a person
unless you separately link it for payouts.

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

Or from source:

```bash
cargo install --git https://github.com/kmplify/kmplify-node
```

Prebuilt binaries, their SHA256SUMS, the systemd unit and the env template
are attached to every [GitHub release](https://github.com/kmplify/kmplify-node/releases).

## Run

Check the configuration before joining anything. This connects to nothing; it
resolves the config, probes Docker, `nvidia-smi` and your model endpoint, and
tells you what the fabric would see:

```bash
kmplify-node check
```

Then join:

```bash
kmplify-node
```

A node that lists no models will connect, count as online, and have every job
refused by the scheduler, so `check` fails loudly on that rather than letting
you deploy something that looks healthy and serves nothing.

To run it as a service on a headless box, including the fully-static musl
build that executes on any Linux distribution, see
[docs/HEADLESS-NODE.md](docs/HEADLESS-NODE.md) and the systemd unit in
[packaging/](packaging/).

## Configuration

Environment variables only, so a service manager, a container and a shell all
configure it the same way.

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
| `KMPLIFY_GPU_BACKEND` | autodetect | Force the accelerator: `cuda`, `rocm`, `oneapi`, `metal`, `cpu`. |
| `KMPLIFY_CUDA` | autodetect | Older CUDA-only override (`1`/`0`). Still honoured. |
| `KMPLIFY_FABRIC_EXTRA_IMAGE_PINS` | *empty* | Extra `template=repository` image pins. See below. |
| `PROVIDER_FUNCTIONS` | `false` | Host signed Wasm functions (needs a build with `--features wasm`). |
| `PROVIDER_FUNCTIONS_PUBKEY` | *empty* | Hex Ed25519 key of the function catalog to trust. Empty = refuse all. |
| `PROVIDER_MAX_FUNCTION_MB` | `256` | Per-call memory ceiling (hard cap 1024). |
| `PROVIDER_MAX_FUNCTION_MS` | `30000` | Per-call wall-clock ceiling (hard cap 300000). |
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
