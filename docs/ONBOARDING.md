# Becoming a provider: the `init` walkthrough

`kmplify-node init` is the guided first run. This page is that run, captured
from a real terminal, with a note on each step about what is being decided
and what can be changed later. Nothing here is special to the wizard: every
answer lands in `settings.json`, the same file `kmplify-node set` and the
dashboard's sharing screen write, so anything chosen here can be re-chosen
any time without running `init` again.

Three properties worth knowing before you start:

- **Nothing is shared until you confirm the summary.** The wizard asks,
  shows you everything it is about to save, and only then writes. Ctrl-C at
  any point, or a closed stdin, abandons the run and saves nothing.
- **Every question has a safe default.** Pressing Enter through the whole
  wizard produces a node that shares inference from the engine it found and
  nothing else.
- **Re-running is safe.** `init` only touches the settings it asks about; a
  rewards switch or a colibri key you set before stays as it is.

```
$ kmplify-node init

◆ kmplify-node setup — lend this machine to the KMPLIFY Compute Fabric
   Seven steps. Nothing is shared until you confirm the summary; Ctrl-C abandons all of it.

 [1/7] this machine
   accelerator : metal · Apple M2 Max · 49152 MB
   cpu         : Apple M2 Max · 12 cores · 64 GB RAM
   This is what the fabric would see. Ceilings on all of it can be set later with `kmplify-node set`.
```

**Step 1** is a mirror, not a question: the accelerator and CPU exactly as
the fabric would see them. If the accelerator line surprises you (a CUDA
card showing as `none`, say), stop here and run `kmplify-node check` — it
explains the difference between a driver being installed and a card being
usable.

```
 [2/7] inference engine
   scanning localhost for running engines…
   running now:
   1) Ollama     http://127.0.0.1:11434       17 model(s)
   2) LM Studio  http://127.0.0.1:1234        28 model(s)
   not running — pick one to set it up for later:
   3) llama.cpp  `llama-server -m model.gguf` serves OpenAI-compatible on :8080
   4) MLX        `mlx_lm.server` serves OpenAI-compatible on :8080 (Apple Silicon)
   5) vLLM       `vllm serve <model>` listens on :8000 (CUDA/ROCm hosts)
   6) LiteLLM    `litellm --model …` proxies many engines on :4000
   7) Jan        Jan → Local API Server (:1337)
   8) somewhere else (enter a URL)
   9) rescan
   10) no engine — lend CPU, functions or vector storage only
   engine [1]: 1
```

**Step 2** offers the whole roster, not just what is running. Engines that
answered are listed first with their model counts; the rest of the roster —
llama.cpp, MLX, vLLM, LM Studio, LiteLLM, Jan — can be picked *for later*:
the choice is saved, and the wizard says plainly that the node will
advertise nothing until that engine answers. Detection is by evidence, not
port: a scan identifies each engine by how it answers, and a model count of
`0` is shown as the warning it is.

- Engine on another machine or port? Option 8, or just paste the URL.
- colibri gets its own question rather than a menu slot, because it is a
  *second* upstream lent alongside your engine, not an alternative to it —
  asked only when something answers where colibri usually runs.

Change it later with `kmplify-node set engine=<name or URL>`, and see the
roster any time with `kmplify-node engines`.

```
 [3/7] what you lend
   share inference from Ollama (17 models)? [Y/n]: 
   lend spare CPU threads and RAM to peers? [y/N]: y
```

**Step 3** is what you lend. Inference means chat and embedding jobs against
your engine. CPU/RAM lending advertises spare threads and memory (unified
memory on Apple Silicon). On hosts whose accelerator can be passed into
Docker (CUDA, ROCm, oneAPI), a third question offers **container sessions**
— peers' vLLM/ComfyUI/Ollama containers on your GPU — with the template list
shown and trimmable later via `set workloads=…`. A Mac is not asked, because
macOS cannot pass a GPU into a container, and Docker being absent skips the
question rather than saving a promise the preflight would fail.

```
 [4/7] ceilings — peers never take more than this (Enter = all of it)
   VRAM offered to peers, e.g. 24g [all 48 GB]: 24g
   system RAM offered to peers, e.g. 32g [all 64 GB]: 32g
```

**Step 4** is the desktop app's sliders, as questions — and only the ones
that could bind anything: session cores and disk appear when sessions are
on, VRAM when there is an accelerator to cap, RAM when CPU sharing is on.
Answers speak the units people think in: `24g`, `32gb`, `1t` or a bare
number in the setting's own unit. Enter keeps a ceiling at *all of it*, and
`kmplify-node set max-cpus=… max-vram-mb=24g …` changes any of them later
without a restart.

```
 [5/7] who may use it
   1) auto — any consumer on the fabric may use this node
   2) manual — unknown consumers wait until you approve them (kmplify-node peers)
   admission [1]: 
   country, ISO alpha-2 — lets EU consumers find you (Enter for none): de
```

**Step 5** is admission and residency. `manual` parks unknown consumers
until you decide — the deciding happens in `kmplify-node peers` or the
dashboard's peers screen, and invitations you mint always connect either
way. The country is self-declared and exists so consumers who want EU/EEA
capacity can find you; leaving it empty records `XX`.

```
 [6/7] extra lanes (both off by default)
   host signed Wasm functions? (small sandboxed jobs: HTML to text, CSV to JSON, …) [y/N]: y
   this fabric signs its catalog with 58c84fcafcc81e04… — only modules under this key will run
   trust it? [Y/n]: 
   hold peers' vector collections? (replicated RAG indexes, payloads opaque to you) [y/N]:
```

**Step 6** is the protocol v3.0 lanes. Opting into functions makes the
wizard fetch the fabric's catalog signing key and show it — trusting that
key is the actual decision, because your node will only ever run modules
signed with it, in a sandbox with stdin/stdout and nothing else. The source
of every catalog module is public in this repository under `functions/`.

```
 [7/7] summary
   engine    : Ollama at http://127.0.0.1:11434
   inference : on
   cpu + ram : on
   sessions  : off
   ceilings  : cpus all · vram 24g · ram 32g · disk all
   admission : auto
   country   : DE
   functions : on
   vectors   : off
   save these choices? [Y/n]: 
   saved to ~/.config/kmplify-node/settings.json — change any of it later with `kmplify-node set` or the dashboard's sharing screen

   preflight…
   ready.
   start lending now? [Y/n]: n

   when you are ready:
     kmplify-node   run in this terminal
     kmplify-node tui   run with the dashboard
     as a service: docs/HEADLESS-NODE.md (systemd unit ships in packaging/)
```

**Step 7** shows everything before anything is written, then runs the same
preflight `kmplify-node check` runs, so `ready.` here means the same thing
it means there. A problem does not throw your answers away. One deliberate
softness: an engine you picked *for later* having no models yet is reported
as the plan it is, not as a problem — the node preflights ready the moment
that engine answers.

## After the wizard

| Want to… | Command |
|---|---|
| watch and steer the node | `kmplify-node tui` |
| see or switch the engine | `kmplify-node engines`, `set engine=…` |
| attach colibri as a second upstream | `set colibri=http://127.0.0.1:5000` |
| change any answer | `kmplify-node set <key>=<value>` (see `help`) |
| move a ceiling | `set max-vram-mb=24g max-ram-mb=32g max-disk-gb=1t` |
| trim the session templates | `set workloads=vllm-openai,comfyui` |
| approve consumers (manual mode) | `kmplify-node peers` |
| re-check readiness | `kmplify-node check` |
| run 24/7 as a service | [HEADLESS-NODE.md](HEADLESS-NODE.md) |

And because the wizard reads answers from stdin, it can be scripted — a
provisioning system can pipe the answers and get the same validation and the
same preflight an interactive run gets.
