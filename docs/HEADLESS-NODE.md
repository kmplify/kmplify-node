# kmplify-node — the headless KMPLIFY provider

A single binary that turns any GUI-less Linux machine into a KMPLIFY fabric
provider: it serves the host's Ollama models to peers and (opt-in) hosts
container sessions — vLLM, ComfyUI, Ollama — on the machine's GPU. No
webview, no chat, no RAG stack, no configuration UI.

It is **the same fabric worker the KMPLIFY desktop app runs**, not a port of
it: container-death reporting, reconnect-safe session state, live capability
inventory, the operator's CPU/VRAM/disk ceilings. There is exactly one
implementation, it lives in this repository, and the desktop app depends on
this crate to get it. A fork would have been a second worker drifting from
day one, which is precisely what the crate dependency exists to prevent.

## Portability: every GUI-less distro

The release build targets `x86_64-unknown-linux-musl` and links fully
statically. The crate is OpenSSL-free (rustls everywhere), so the binary has
**zero runtime library dependencies** — no glibc version to match, nothing
to install from any distro's repositories to *execute* it. Ubuntu, Debian,
openSUSE, CachyOS/Arch, Alpine, a container `FROM scratch`: if the kernel
runs, the binary runs.

What the *host* needs is about what the node does, not what it links:

| Capability | Requirement |
|---|---|
| execute the binary | nothing |
| serve host Ollama models | Ollama running on the host (`OLLAMA_BASE`) |
| host container sessions | Docker daemon + the node's user in the `docker` group |
| host CUDA sessions (vLLM/ComfyUI) | NVIDIA driver + [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html) |

Distro one-liners for the toolkit prerequisites:

- **Ubuntu/Debian**: `apt install docker.io nvidia-container-toolkit`
- **openSUSE**: `zypper install docker nvidia-container-toolkit`
- **CachyOS/Arch**: `pacman -S docker nvidia-container-toolkit`

vLLM-based templates require **native Linux**: they cannot run under WSL2,
because vLLM's V1 engine needs UVA and WSL2's GPU paravirtualization lacks
it. A native-Linux CUDA box running this binary is the highest-value kind of
node a fabric can have.

## Build

```sh
# Native (any OS, for development):
cargo build --release

# Fully-static Linux release (needs only Docker):
docker build -f packaging/Dockerfile.node-build -o dist-node .
# -> dist-node/kmplify-node
```

Nothing here links a webview or GTK, so it compiles on machines where those
system libraries do not exist. That used to require a feature flag, back when
this binary was built from inside the desktop app's crate.

## Configure

Environment variables, deliberately the same names the desktop app writes
into its stack `.env`:

| Variable | Default | Meaning |
|---|---|---|
| `PROVIDER_GATEWAY_URL` | the public fabric | gateway to join |
| `PROVIDER_WORKLOADS` | *(empty = sessions off)* | template ids to host, e.g. `vllm-openai,vllm-openai-lmcache,comfyui,ollama,echo-test`. Running other people's containers is opt-in here exactly as in the app. |
| `PROVIDER_MAX_CPUS` | half the host | ceiling on CPUs lent to sessions |
| `PROVIDER_MAX_VRAM_MB` | card total | ceiling on advertised VRAM |
| `PROVIDER_MAX_DISK_GB` | unbounded | ceiling on disk sessions may fill |
| `PROVIDER_COUNTRY` | *(undeclared)* | ISO alpha-2, for EU-only consumers |
| `OLLAMA_BASE` | `http://127.0.0.1:11434` | host Ollama to serve models from |
| `KMPLIFY_NODE_DIR` | `~/.config/kmplify-node` | identity/credentials directory |
| `KMPLIFY_CUDA` | autodetected | force CUDA advertising `1`/`0` |

Always start with the preflight — it prints the resolved configuration and
probes docker/nvidia-smi/Ollama without connecting, and exits non-zero when
sessions are offered but Docker is unreachable, so provisioning scripts fail
loudly instead of deploying a broken node:

```sh
kmplify-node check
```

## Run as a service

```sh
install -m 755 dist-node/kmplify-node /usr/local/bin/kmplify-node
useradd -r -G docker -d /var/lib/kmplify-node kmplify
install -m 644 packaging/kmplify-node.service /etc/systemd/system/
# ceilings/templates go in /etc/kmplify-node.env (see the unit's EnvironmentFile)
systemctl enable --now kmplify-node
```

`systemctl stop` sends SIGTERM; the worker tears down every hosted session
container before exiting — a stopped node leaves nothing running on the
owner's GPU. The unit works unchanged on every systemd distribution.

## Scope, honestly

- Serves: fabric container sessions (vLLM / ComfyUI / Ollama / echo-test
  templates) and host-Ollama model inference.
- Does **not** serve LlamaCPP: that is a backend engine, not a fabric
  template. It joins this node the day someone adds a llamacpp template to
  the gateway catalog, which is a template addition rather than a node
  change.
- The identity in `KMPLIFY_NODE_DIR` is per-gateway and self-heals: pointing
  an existing node at a different gateway re-registers automatically.
