# Earning KMPLIFY Credits with this node

A node that hosts rented GPU sessions (a GPU app such as ComfyUI or vLLM, or a
container session) earns **KMPLIFY Credits** for the time a renter paid for.
This page says what that means, what it does not mean, and what the node shows.

> **Status.** The credit system is switched on at the gateway, not here. Until
> the fabric you joined has it on, the node shows nothing about credits and
> behaves exactly as it always has. Nothing here is an income promise: a node
> earns only for sessions somebody actually rented and paid for.

## What credits are

- **A prepaid balance for KMPLIFY services.** Credits pay for rented GPU time
  and other metered KMPLIFY services, inside KMPLIFY.
- **Not a currency.** They cannot be sent to another person, traded or
  exchanged.
- **No cash payout.** Earned credits are spent on KMPLIFY services. There is
  no way to turn them into money, and this node has no payout path for them.
- **Valid for 24 months** from the day they were earned.

Someone who lends a GPU at night can rent a bigger one by day with what the
night earned. That is the whole idea.

## What earns, and what does not

| Earns credits | Does not earn credits |
|---|---|
| Rented sessions on your GPU: GPU apps from the template catalog and container sessions, per metered second | Chat, embedding and other inference jobs, functions, vector collections |

Inference on the fabric costs the people who use it no credits, so there is
nothing to pass on for it. A node that lends only inference earns none.

A node is credited only for seconds the renter actually paid for. If a session
was not paid (it failed to start, or the time was refunded), nothing is earned
for that time. What the renter is charged and what the node earns come from
the same metered seconds on the gateway; the node's own counters settle
nothing.

## The rate and the fee

The price of a session depends on the **GPU class** of the machine that hosts
it. The gateway puts a node in one of four classes, S, M, L or XL, by the VRAM
it lends: the more VRAM, the higher the class. `kmplify-node status` shows
the class your node is in.

Each class has a rate in credits per minute, charged to the renter. KMPLIFY
keeps a **platform fee**, a percentage of that, and the rest is credited to
the node's account.

The rate and the fee are set by KMPLIFY in one place on the server and can
change. That is why no number for them is written in this repository: the
node shows the rate of its class and the fee as the gateway reports them at
that moment.

Two settings on your side decide the class:

- `max-vram-mb` (`PROVIDER_MAX_VRAM_MB`): the node advertises at most this
  much per card, and the class follows what is advertised, not what the card
  could do.
- `country` (`PROVIDER_COUNTRY`): the region. Renters in EU mode book EU and
  EEA nodes only. A node that declares no country gets one derived from its
  connecting address.

## Linking the node to a KMPLIFY account

A node is anonymous on the fabric. Credits need somewhere to go, so earnings
are booked to the KMPLIFY account the node is **linked** to.

**Until a node is linked, its earnings wait.** The gateway holds them for the
node and moves them to the account when the link is made. Nothing is lost by
linking late, within the validity above.

To link:

1. On the node, print its public address:
   ```bash
   kmplify-node id
   ```
   The second line starts with `address: kmpn1…`. This is the public half of
   the node's identity key. It is not a credential and it is safe to copy.
2. In the KMPLIFY app (web or desktop), signed in to the account that should
   receive the credits, open **Settings**, **Your KMPLIFY identity**,
   **My nodes**.
3. Paste the address and choose **Claim node**.

The link is a statement signed in your account. The node itself is not told a
name, an e-mail address or an account id, and it stays anonymous to the people
who use it.

## What the node shows

Everything below appears only when the gateway reports it.

`kmplify-node status` gains three lines:

```
ONLINE pid 30736 up 3h 12m
  node     : 8f2c1a5b90de4a17b3c9d0e5f6a7b8c9
  …
  credits  : 1240 earned this month · 5678 in total
  rate     : class M (24 GB) · <rate> credits per minute · platform fee <fee> % · about <share> per minute to this node
  account  : linked
```

On a node that is not linked, the last line says so:

```
  account  : NOT LINKED: 420 credits wait until it is (`kmplify-node rewards` says how)
```

`kmplify-node rewards` asks the gateway live and shows the same figures, the
latest earnings (time, session, class, paid seconds, credits and the fee
taken), and on an unlinked node the three steps above with the address
already printed.

`kmplify-node status --json` and `status.json` carry a `credits` block:

```json
"credits": {
  "enabled": true,
  "linked": true,
  "gpu_class": "m",
  "vram_gb": 24,
  "credits_per_minute": 0,
  "platform_fee_percent": 0,
  "earned_this_month": 1240,
  "earned_total": 5678,
  "held": 0,
  "history": [
    {"at": "2026-11-21T20:14:03Z", "credits": 153, "session_id": "…",
     "gpu_class": "m", "seconds": 1800, "fee_credits": 27}
  ]
}
```

(`credits_per_minute` and `platform_fee_percent` hold the gateway's current
values; the zeros above are placeholders.) The block is absent, key and all,
while the gateway has the credit system off.

The dashboard (`kmplify-node tui`) shows one `credits` line on its home
screen.

The node does **not** show the account's balance. A node runs on machines
other people may administer; what an account holds is the account's business
and is shown in the KMPLIFY apps.

## How the node asks

One read-only request, with the token the node already uses for the gateway:

```
GET <gateway>/fabric/credits/node
Authorization: Bearer <node token>
```

- A running node asks every five minutes, and every fifteen while the answer
  is "off". `rewards` asks once, when you run it.
- `enabled: false`, or a gateway that does not know the path, means "show
  nothing".
- An answer that cannot be read (network, a server error) leaves the last
  figures in place and is tried again later.
- The token is sent to the gateway the node registered with, never to another
  host, and never over plain http to a public address.

The answer's fields are in [PROTOCOL.md](../PROTOCOL.md#credits-what-a-node-earned-read-only).

## What this never does

- The node books nothing, holds no balance and cannot spend credits.
- Scheduling, admission, template pins and consent never depend on credits. A
  node that could not reach the endpoint serves exactly as before.
- The node does not meter what it is paid on. Its `delivered` counters are for
  noticing a disagreement, not for settling one.

## Credits and the rewards companion

They are two different things. [REWARDS.md](REWARDS.md) describes an optional
local program for payouts on a rail that program owns, switched on with
`set rewards=on`. Credits need no companion and no setting: they are reported
by the fabric itself and are spent inside KMPLIFY.
