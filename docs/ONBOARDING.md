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
  rewards switch, ceilings or a colibri key you set before stay as they are.

```
$ kmplify-node init

 ◆ kmplify-node setup — lend this machine to the KMPLIFY Compute Fabric
   Six steps. Nothing is shared until you confirm the summary; Ctrl-C abandons all of it.

 [1/6] this machine
   accelerator : metal · Apple M2 Max · 49152 MB
   cpu         : Apple M2 Max · 12 cores · 64 GB RAM
   This is what the fabric would see. Ceilings on all of it can be set later with `kmplify-node set`.
```

**Step 1** is a mirror, not a question: the accelerator and CPU exactly as
the fabric would see them. If the accelerator line surprises you (a CUDA
card showing as `none`, say), stop here and run `kmplify-node check` — it
explains the difference between a driver being installed and a card being
usable. Ceilings on cores, VRAM, RAM and disk are deliberately not asked
here; `kmplify-node set max-cpus=6 …` sets them when you want them.

```
 [2/6] inference engine
   scanning localhost for running engines…
   1) Ollama     http://127.0.0.1:11434       17 model(s)
   2) LM Studio  http://127.0.0.1:1234        28 model(s)
   3) somewhere else (enter a URL)
   4) rescan
   5) no engine — lend CPU, functions or vector storage only
   engine [1]: 1
```

**Step 2** finds your engine instead of asking for its address. The scan
covers the usual localhost ports of Ollama, llama.cpp, vLLM, LM Studio,
LiteLLM, Jan and colibri, and identifies each by how it answers, not by
which port it sits on. A model count of `0` is shown as the warning it is:
that engine is online but would refuse every job.

- Engine on another machine or port? Pick *somewhere else* and paste the
  URL — anything speaking the OpenAI API works.
- Just started the engine? *rescan*.
- No engine at all? Option 5 is a real choice, not a dead end: a node can
  lend CPU and RAM, host signed functions or hold vector collections
  without serving a single model.

Change it later with `kmplify-node set engine=<name or URL>`, and see what
is running any time with `kmplify-node engines`.

```
 [3/6] what you lend
   share inference from Ollama (17 models)? [Y/n]:
   lend spare CPU threads and RAM to peers? [y/N]:
```

**Step 3** is the two basic grants. Inference means chat and embedding jobs
against your engine; prompts and responses pass through your machine and the
model runs on your hardware. CPU/RAM lending advertises spare threads and
memory (unified memory on Apple Silicon) and is off by default.

```
 [4/6] who may use it
   1) auto — any consumer on the fabric may use this node
   2) manual — unknown consumers wait until you approve them (kmplify-node peers)
   admission [1]:
   country, ISO alpha-2 — lets EU consumers find you (Enter for none): de
```

**Step 4** is admission and residency. `manual` parks unknown consumers
until you decide — the deciding happens in `kmplify-node peers` or the
dashboard's peers screen, and invitations you mint always connect either
way. The country is self-declared and exists so consumers who want EU/EEA
capacity can find you; leaving it empty records `XX`, which makes this node
invisible to every EU-only search.

```
 [5/6] extra lanes (both off by default)
   host signed Wasm functions? (small sandboxed jobs: HTML to text, CSV to JSON, …) [y/N]: y
   this fabric signs its catalog with 58c84fcafcc81e04… — only modules under this key will run
   trust it? [Y/n]:
   hold peers' vector collections? (replicated RAG indexes, payloads opaque to you) [y/N]:
```

**Step 5** is the protocol v3.0 lanes. Opting into functions makes the
wizard fetch the fabric's catalog signing key and show it to you — trusting
that key is the actual decision, because your node will only ever run
modules signed with it, in a sandbox with stdin/stdout and nothing else. The
source of every catalog module is public in this repository under
`functions/`. Vector collections are peers' RAG indexes, replicated onto
your disk with payloads that are opaque bytes to you.

```
 [6/6] summary
   engine    : Ollama at http://127.0.0.1:11434
   inference : on
   cpu + ram : off
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
     kmplify-node       run in this terminal
     kmplify-node tui   run with the dashboard
     as a service: docs/HEADLESS-NODE.md (systemd unit ships in packaging/)
```

**Step 6** shows everything before anything is written, then runs the same
preflight `kmplify-node check` runs — gateway reachability, engine and model
list, the lanes' keys — so `ready.` here means the same thing it means
there. A problem does not throw your answers away: the wizard says what is
wrong, keeps the settings, and `kmplify-node check` re-judges after you fix
it.

Answering `Y` to *start lending now* runs the node in this terminal until
Ctrl-C, which also tears down anything peers were running. `n` leaves you
with the three ways to start when you are ready.

## After the wizard

| Want to… | Command |
|---|---|
| watch and steer the node | `kmplify-node tui` |
| see or switch the engine | `kmplify-node engines`, `set engine=…` |
| change any answer | `kmplify-node set <key>=<value>` (see `help`) |
| set ceilings on what peers may take | `set max-cpus=… max-vram-mb=… max-ram-mb=… max-disk-gb=…` |
| approve consumers (manual mode) | `kmplify-node peers` |
| re-check readiness | `kmplify-node check` |
| run 24/7 as a service | [HEADLESS-NODE.md](HEADLESS-NODE.md) |

And because the wizard reads answers from stdin, it can be scripted — a
provisioning system can pipe the answers and get the same validation and the
same preflight an interactive run gets.
