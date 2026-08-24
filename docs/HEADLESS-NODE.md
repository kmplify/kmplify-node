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

## Install

The fast path, which fetches the release binary for this machine, verifies
it against SHA256SUMS, installs the systemd unit and env template, and runs
the preflight:

```sh
curl -fsSL https://raw.githubusercontent.com/kmplify/kmplify-node/main/scripts/install.sh | sudo sh -s -- --service
```

Then edit `/etc/kmplify-node.env` (ceilings, templates, country) and:

```sh
systemctl enable --now kmplify-node
```

An inference-only node can instead run as a container; see "Docker" below.

## Build

```sh
# Native (any OS, for development):
cargo build --release

# Fully-static Linux release (needs only Docker; builds the host's
# architecture, x86_64 or aarch64):
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
| `PROVIDER_WORKLOADS` | *(empty = sessions off)* | template ids to host, e.g. `vllm-openai,vllm-openai-lmcache,comfyui,ollama,speaches,echo-test`. Running other people's containers is opt-in here exactly as in the app. |
| `PROVIDER_MAX_CPUS` | half the host | ceiling on CPUs lent to sessions |
| `PROVIDER_MAX_VRAM_MB` | card total | ceiling on advertised VRAM |
| `PROVIDER_MAX_DISK_GB` | unbounded | ceiling on disk sessions may fill |
| `PROVIDER_COUNTRY` | *(undeclared)* | ISO alpha-2, for EU-only consumers |
| `OLLAMA_BASE` | `http://127.0.0.1:11434` | host Ollama to serve models from |
| `KMPLIFY_NODE_DIR` | `~/.config/kmplify-node` | identity/credentials directory |
| `KMPLIFY_CUDA` | autodetected | force CUDA advertising `1`/`0` |

Always start with the preflight — it prints the resolved configuration and
probes docker/nvidia-smi/the gateway/Ollama without connecting, and exits
non-zero when the host cannot serve as configured, so provisioning scripts
fail loudly instead of deploying a broken node:

```sh
kmplify-node check          # 0 ready · 1 cannot serve · 2 bad configuration
kmplify-node check --json   # the same verdict, for a script
```

Every variable above also has a flag that sets it (`--gateway`,
`--workloads`, `--max-vram-mb`, `--no-share-inference`, …), which is handy
for trying a setting before writing it into a unit file:

```sh
kmplify-node check --gateway https://fabric.example --workloads vllm-openai
```

## The terminal dashboard

`kmplify-node tui` is how a GUI-less machine is operated: it shows link
state, advertised models, the peers running here and the log, and it drives
the node — pause and resume sharing (`p`), reconnect (`c`), evict a session
(`e`), stop the node (`x`), write a snapshot for a bug report (`w`).

```sh
kmplify-node tui
```

It attaches to the node already running on this machine and leaves it running
when you quit. With no node running it starts one, and quitting stops it.

The node publishes `status.json` in `KMPLIFY_NODE_DIR` (mode 0600) and reads
commands dropped as files into `control/` there. Nothing listens on a port,
which also means the dashboard must run as the user that owns that directory.
For the systemd install below, that is the `kmplify` user:

```sh
sudo -u kmplify KMPLIFY_NODE_DIR=/var/lib/kmplify-node kmplify-node tui
```

### What this machine lends (key `5`)

The sharing screen is the desktop app's "Provide this machine's Resources"
panel: switches for inference, per-template container sessions, CPU/RAM and
manual approval; ceilings for cores, VRAM, RAM and disk; country and colibri
upstream. `space` toggles, `←/→` moves a ceiling (`shift` for a bigger step),
`enter` edits a field, `d` hands a row back to the environment, `s` applies —
which reconnects the node so the fabric hears the new terms. Hosted sessions
survive it.

The same without a terminal:

```sh
kmplify-node set max-cpus=6 share-cpu=true
kmplify-node set workloads=vllm-openai,comfyui   # empty value = sessions off
kmplify-node set --clear max-cpus                # back to /etc/kmplify-node.env
kmplify-node set --list
```

Both write `settings.json` in `KMPLIFY_NODE_DIR` (mode 0600 — it can hold the
colibri key) and signal the running node, which re-advertises within a second;
a node that is offline picks the change up on its next connection. **A stored
choice overrides the environment**, so an operator standing at the machine
beats a unit file written months ago. `kmplify-node check` prints every such
override, and `set --clear KEY` removes one.

### Activity monitor (key `7`)

CPU, GPU, VRAM and RAM live, each with a bar and five minutes of history, a
bar per logical CPU, and what the fabric is holding right now (sessions, cores
promised to peers, jobs, disk). The home screen carries the same four meters
in miniature.

GPU utilization comes from `nvidia-smi` / `rocm-smi` and is sampled every few
seconds — a node lending its cycles should not spend them on being watched.
Where a platform will not report a figure (macOS exposes GPU load only to
privileged tools) the panel says so instead of drawing a zero.

### Who may use it (key `6`)

Consumers waiting for a decision (`a` approve, `n` deny, `b` block, `u` clear
the rule), consumers seen recently and how their work arrived, and the
invitations this node has minted (`i` new, `h` hold or resume, `v` revoke).

Turning on manual approval without this screen would be a trap: the node
would park every unknown consumer and nobody at the terminal could let them
in. The screen talks to the gateway as the node, with the credential in
`KMPLIFY_NODE_DIR`; an unreachable gateway costs the screen and nothing else.

### Admission from a script

The mode is a setting like any other, and the decisions have their own verbs,
so a machine with no terminal in front of it is still yours to govern:

```sh
kmplify-node set approval-mode=manual   # unknown consumers wait for you
kmplify-node peers                      # who is waiting, who is using it
kmplify-node peers approve node-9abc    # standing rule: they are in
kmplify-node peers block anon-1a2b3c4d  # refused in every mode
kmplify-node peers clear node-9abc      # back to whatever the mode says
kmplify-node peers invite "Anna's phone"   # prints the invitation id
kmplify-node peers revoke <invitation-id>
```

`peers` talks to the gateway with the node's stored credential, so it works
whether or not the node is currently running — approving from cron is fine.
Invitations bypass manual admission by design: minting one is the approval.

For scripts and monitoring, the same snapshot without the full screen:

```sh
kmplify-node status          # human-readable, exit 1 when no node is running
kmplify-node status --json   # every field, including live CPU/RAM/VRAM
```

## Docker (inference-only)

The published image serves the host's models but cannot host container
sessions: a session host must drive the host's Docker daemon and see its
GPU, which a containerised worker cannot. The worker detects that and
advertises itself inference-only, so this never promises the fabric
something the node cannot deliver.

```sh
docker run -d --name kmplify-node --restart unless-stopped \
  --network host \
  -v kmplify-node-data:/data \
  -e PROVIDER_COUNTRY=DE \
  ghcr.io/kmplify/kmplify-node
```

`--network host` (Linux) lets the default `OLLAMA_BASE` reach the host's
Ollama; elsewhere set `OLLAMA_BASE=http://host.docker.internal:11434`. The
volume keeps the node's anonymous identity across container replacements;
without it every recreation joins the fabric as a brand-new peer.

The dashboard attaches from inside the container, where the node directory
and the running worker are:

```sh
docker exec -it kmplify-node kmplify-node tui
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

## When `check` and your driver disagree

`check` asks two different questions about the GPU, and prints both when the
answers differ:

```
accelerator
  advertised : cpu (no accelerator offered to the fabric)
  installed  : ROCm (AMD) driver tooling is present
               but its tool did not answer, so nothing is advertised
               for it. Local inference may still work; the fabric
               only advertises what it can size and serve.
```

That is not a bug report, it is a diagnosis. The node found the ROCm stack on
disk but `rocm-smi` did not return usable output, so it will not promise a
consumer capacity it cannot size. Usual causes, cheapest first:

- The node's user cannot reach the device. On Linux the account needs the
  `video` (and often `render`) group; the systemd unit runs as `kmplify`, so
  it is that user's groups that matter, not yours.
- The tool is installed but not on the service's PATH. `check` run from your
  shell and the unit run by systemd do not share an environment.
- The driver is genuinely broken or mismatched with the kernel, which
  `rocm-smi` / `nvidia-smi` will say directly if you run it as the node's user.

The reverse case, `advertised` naming a backend that was **NOT DETECTED**,
means `KMPLIFY_GPU_BACKEND` is forcing something this host does not have. The
hello frame reports `cpu` in that case rather than a phantom card, because
the scheduler reads advertised VRAM as capacity.

## Scope, honestly

- Serves: fabric container sessions (vLLM / ComfyUI / Ollama / speech STT+TTS /
  echo-test templates) and host-Ollama model inference.
- Does **not** serve LlamaCPP: that is a backend engine, not a fabric
  template. It joins this node the day someone adds a llamacpp template to
  the gateway catalog, which is a template addition rather than a node
  change.
- The identity in `KMPLIFY_NODE_DIR` is per-gateway and self-heals: pointing
  an existing node at a different gateway re-registers automatically.
