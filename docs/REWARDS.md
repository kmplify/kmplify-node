# Getting paid for what this node serves

kmplify-node lends hardware. It does not hold a wallet, sign a transaction,
know a token or meter anything it could be paid on. Being paid is a separate
program that an operator installs on purpose — today the **Chaingence payment
plugin** — and this document is the contract between the two.

Everything here is optional in the strongest sense: a node with no companion
installed, and a node whose operator never switches this on, behaves exactly
as it always has. Nothing about serving depends on it.

> **Status.** The payout side is a testnet-only concept in another repository.
> This document specifies what the node publishes and how a companion is
> asked. No node earns anything today, and nothing here should be read as an
> income promise: rewards depend on delivered, verified, demanded compute.

## The boundary

1. **kmplify-node stays token-free.** No wallet, no key material, no address,
   no chain client. The node's half is publishing facts about itself.
2. **One direction only.** KMPLIFY works completely without any of this. A
   companion consumes what the node and the fabric publish; neither ever asks
   a companion for permission.
3. **The chain is a payout rail, never a control plane.** Scheduling,
   admission, template pins and consent never read a payment system. A node
   that stopped serving because a payment plugin was unhappy would be exactly
   the thing this rule exists to prevent.
4. **The fabric attests, the companion settles.** What a node says about its
   own work cannot settle anything; the gateway's signed receipts are the
   record. The node's own counters exist so an operator can *notice a
   disagreement*, not so anyone can be paid from them.

## What the node publishes

Both files live in `KMPLIFY_NODE_DIR` (`~/.config/kmplify-node` by default).

### `identity.json` — the public half of the node's identity

```json
{
  "schema": 1,
  "node_id": "44802ebc90e24de18825b90af320edfe",
  "gateway": "https://fabric.kmplify.io",
  "version": "0.3.0+c8ba618",
  "os": "macos",
  "arch": "aarch64",
  "published_at_ms": 1787608216832
}
```

Written whenever the node establishes its identity, world-readable, and it
contains **nothing secret**: the node id travels in every hello frame and the
gateway is a URL.

It exists because of one specific thing a companion must never be taught to
do. The node's credential file, `fabric_node.json`, holds the node id *and*
the gateway token — the token IS the node — and it is mode `0600` for that
reason. A companion that reads it to learn an id is one bug away from
exfiltrating the node itself.

> **Companions read `identity.json`. Never `fabric_node.json`.**

A companion that is running before the node has ever registered will find no
file; that is the correct answer, and binding an account to an empty id is
not a thing anyone should do.

### `status.json` — what the node is doing, including what it delivered

Owner-only (it carries the log tail), and already the dashboard's data source.
It gained a `delivered` block for this:

```json
"delivered": {
  "jobs": 914,
  "job_ms": 742100,
  "sessions": 3,
  "session_seconds": 51840,
  "since_ms": 1787600000000
}
```

Counted since the process started, and deliberately **not** persisted. A
lifetime total kept by a node about its own earnings would be a ledger, and a
ledger the earner maintains is one nobody should trust or be asked to
maintain. Read it as "what this machine believes it served", next to what the
fabric attests.

`kmplify-node status --json` prints the same structure, so a companion never
has to read files at all if it would rather run a command.

## How a companion is asked

Two deliberate acts, because running another program is not something a node
should do because a binary happened to be on `PATH`:

```bash
# 1. install the companion (its own repository, its own licence)
# 2. tell this node it may ask
kmplify-node set rewards=on
```

Then:

```bash
kmplify-node rewards
```

```
this node
  node id  : 44802ebc90e24de18825b90af320edfe
  gateway  : https://fabric.kmplify.io
  published: ~/.config/kmplify-node/identity.json

delivered since this node started (6h 12m)
  jobs     : 914 answered in 742.1 s of compute
  sessions : 3 hosted, 14h 24m of machine time
  (the node's own count — the fabric's signed receipts are what settle)

rewards companion
  /usr/local/bin/chaingence-plugin
  TESTNET  12.40 tEURC pending  ·  0.00 tEURC paid  ·  evm:base-sepolia
  account  : plugin-7f9b2c9e
  paid to  : 0x1a2b…c4d5
```

The dashboard shows the short form on its home screen, refreshed every couple
of minutes — a balance is not a live metric, and asking costs a process spawn.

### The call

The node runs exactly one thing:

```
<companion> status --json --node-dir <KMPLIFY_NODE_DIR>
```

- read-only, with a 5 second ceiling, and the child is killed if it overruns;
- no credential, no token and no gateway-supplied argument is ever passed;
- a non-zero exit means the companion's own first line of stderr is shown to
  the operator verbatim — it knows why it is unhappy ("not logged in", "no
  destination yet") far better than the node does.

`CHAINGENCE_PLUGIN=/path/to/binary` names the companion explicitly; otherwise
the node looks for `chaingence-plugin` the same careful way it looks for
`docker`.

### What it must answer with

JSON on stdout. Every field is optional and unknown fields are ignored, so a
companion can grow without waiting for a node release:

| Field | Type | Meaning |
|---|---|---|
| `linked` | bool | is this node bound to an account at all |
| `account` | string | account or plugin id, as the companion wants it shown |
| `destination` | string | payout destination, **already redacted by the companion** |
| `rail` | string | e.g. `evm:base-sepolia`, `sepa` |
| `testnet` | bool | true while the rail is a test network |
| `pending` | string | accrued but unpaid, pre-formatted (`"12.40 tEURC"`) |
| `paid` | string | paid out so far, same rules |
| `note` | string | one sentence for the operator |

Amounts are strings the companion formatted, not numbers the node rounds:
money it does not own is not the node's to render.

`testnet: true` is shown before the number, everywhere, and survives every
truncation. A balance that cannot be spent must never be displayed as one that
can.

## What the node will never do

- store, generate or transmit a key, seed or address;
- take a payout instruction from the gateway, or any instruction at all from a
  companion;
- make scheduling, admission, pricing or consent depend on rewards;
- treat a companion's numbers as an accounting record, or its absence as a
  fault.

If a companion is missing, misconfigured, slow or wrong, the node keeps
serving and says so in one line.

## For companion authors

- Read `identity.json` for the node id and the fabric. Never open
  `fabric_node.json`.
- Read `status.json` (or `kmplify-node status --json`) for delivered work,
  uptime and whether the node is online at all.
- Implement `status --json --node-dir <dir>` as a read-only command that exits
  non-zero with a human sentence on stderr when it cannot answer.
- Bind to the node id, not to a hostname, a MAC address or a file path: the
  node id is the identity the fabric attests against, and it survives moving
  the machine.
- Do not ask the operator to run anything as root, and do not write into the
  node directory. It belongs to the node.

The reference companion is the Chaingence payment plugin, which lives in its
own repository under its own licence. Its wire contract with the rewards API
(binding, attestations, rails) is documented there — none of it is this node's
business, which is the point.
