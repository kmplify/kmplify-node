# The LAN router and the app window

kmplify-node can show the kmplify-nodes on your local network side by side:
their hardware, their live load and what they serve. This is the **LAN
router**. Its first phase is here; routing requests between the machines
follows (see "What comes next").

The **app window** is the node's native window: the same sections, figures
and buttons as the web dashboard and the terminal view. The LAN router is one
of those sections in all three views.

## The app window

![The app window, LAN router section, dark scheme](images/app-window-router-dark.png)

```
kmplify-node --ui app        # start the node with its window (also: --ui native)
kmplify-node window          # open the window of the node already running here
```

In the web dashboard and the terminal view, **App window** (key `a`) opens
it while the node runs. The window is a process of its own that attaches to
the node the way `kmplify-node tui` does: closing it leaves the node running,
and stopping the node closes it.

Where the window cannot open (no display, or a build without the
`native-gui` feature such as the static Linux release binaries), the node
opens the dashboard in a browser app window instead (Edge, Chrome, Chromium
or Brave in app mode), and says so.

Linux needs the display libraries at runtime: libGL/libEGL, libxkbcommon and
X11 or Wayland client libraries, which every desktop installation has.

The window uses Inter, the KMPLIFY text face (SIL Open Font License 1.1, see
`src/native/fonts/OFL.txt`), follows the system's light or dark setting, and
draws nothing that is not in the node's own status file.

## The LAN router

Off until you choose:

| Mode | What listens | Who may connect |
|---|---|---|
| `off` (default) | nothing | nobody |
| `local` | `127.0.0.1:14418` | programs on this computer, with the cluster key |
| `lan` | `0.0.0.0:14418` and an mDNS announcement | addresses on the allowlist, with the cluster key |

```
kmplify-node set router=local
kmplify-node set router=lan router-allow=192.168.1.0/24
kmplify-node router            # what it is doing
kmplify-node router pair       # show a six-digit PIN for two minutes (holds or makes the key)
kmplify-node router join --pin --at 192.168.1.20:14419   # on the other device: enter the PIN
kmplify-node router pair --lan # pairing mode: on the network (allowlist only) for the PIN's two minutes
kmplify-node router key        # show the cluster key (the old way; pairing needs no key shown)
kmplify-node router join       # paste a key (read from stdin)
kmplify-node router new-key    # replace it; every other node must join again
kmplify-node set router-name=studio   # how this node is called to the others
```

The same settings exist as environment variables: `KMPLIFY_ROUTER`,
`KMPLIFY_ROUTER_ALLOW`, `KMPLIFY_ROUTER_NAME`, `KMPLIFY_ROUTER_PORT`
(default 14418) and `KMPLIFY_ROUTER_PAIR_PORT` (default: the router port
plus one, 14419). A running node follows changes within a second.

### In every view

The **LAN router** section of the web dashboard, the terminal view (key `o`)
and the app window shows the same three panels (This router, Nodes on the
network, Router log) from one model (`views::router_model`) and changes the
same things through one action layer (`views::perform`):

| What | Web dashboard | Terminal view | App window |
|---|---|---|---|
| Mode off, local, lan | mode buttons | `M` cycles | mode buttons |
| Allowlist | field, Save | `A` | field, Save |
| Name | field, Save | `N` | field, Save |
| Pair a device | button: PIN, countdown, address | `P` | button: PIN, countdown, address |
| Close pairing | button | `P` again | button |
| Join with PIN | address and PIN fields | `I` (address, then PIN) | address and PIN fields |
| Join a cluster | password field | `J` (typed masked) | password field |
| New cluster key | button, asks first | `K`, asks first | button, asks first |

The views show the key's **fingerprint** only. The key itself is shown by
`kmplify-node router key` on the machine, never by a view (owner decision
2026-11-23: "Show key: no"), and a key typed into a view is stored and
forgotten, never echoed or logged. **Pairing** is the way to add a device:
the PIN lives in the memory of the process that shows it (the node for the
web dashboard, the window, the terminal view, the CLI or kmplify-desktop),
never in a file, and is shown only while it is valid.

### Pairing (protocol v2)

`Pair a device` opens an invitation in the process that shows it: a
six-digit PIN (uniform, from the OS random source), valid for **2 minutes**
and for **3 wrong PINs**; a device that joins closes it. The listener is on
`KMPLIFY_ROUTER_PAIR_PORT` (14419):

* on 127.0.0.1 (a device on this computer, for example kmplify-desktop
  beside a node), unless
* the router is in LAN mode with an allowlist, or `router pair --lan` asks
  for pairing mode: then on every interface for the allowlist only, for the
  PIN's two minutes. Pairing is never announced by mDNS; the joining device
  types the address the PIN screen shows. The joining device only dials
  loopback or an address on its own allowlist.

The exchange (`src/router/pairing.rs`):

1. joiner -> `POST /v2/pair/start {v: 2, msg: A, name}`, `A` = SPAKE2
   (Ed25519 group) message with the PIN, identities
   `kmplify-router-v2/pair/joiner` and `.../inviter`;
2. inviter -> `{v: 2, session, msg: B}`;
3. both: `K` = SPAKE2 key, `th` = SHA-256 over a label, `A`, `B` and the
   session (each length-prefixed), and HKDF-SHA256(salt `th`, ikm `K`) gives
   a joiner confirmation key, an inviter confirmation key and a seal key;
4. joiner -> `POST /v2/pair/finish {session, confirm: HMAC(kj, th)}`. A wrong
   confirmation is a wrong PIN: one of the three attempts, and nothing else
   is said;
5. inviter -> `{confirm: HMAC(ki, th), fingerprint, name, nonce, sealed}`,
   `sealed` = AES-256-GCM(seal key, nonce, cluster key, aad = `th` | fingerprint);
6. joiner checks the inviter's confirmation, opens the key, checks its
   fingerprint, stores it in its key store (`router.key`, or the desktop's
   credentials file). Both screens show the fingerprint for the operator to
   compare.

Properties: a passive listener learns nothing it can test PINs against; an
active attacker on either side gets one guess per session, and each guess
against the inviter costs one of three attempts (the joiner confirms first,
and starting sessions without finishing them reveals nothing). An attacker
on the allowlist can void a PIN by spending its attempts (open a new one).

**Crate:** `spake2` 0.4 (RustCrypto PAKEs, MIT OR Apache-2.0), built on
`curve25519-dalek`, `sha2` and `hkdf`. Its README states it has **not** had an
independent security audit; no RustSec advisory is filed against it
(checked 2026-11-23). There is no audited SPAKE2 crate for Rust; the
audited alternative is `opaque-ke` (NCC Group, 2021, an older major), an
augmented PAKE built for password logins. The exchange is isolated in
`pairing.rs` so the PAKE can be replaced without touching the views.

### Mutual TLS between members (protocol v2)

From v2 a node announces `v=2` in its mDNS record and asks v2 peers over
**TLS 1.3 with a certificate on both sides** (`src/router/tls.rs`):

* each router start makes a fresh ECDSA P-256 key pair and a self-signed
  certificate (rcgen), in memory only (nothing new to store, also for
  kmplify-desktop);
* the certificate's subject alternative name carries the binding tag
  `HMAC-SHA256(cluster key, "kmplify-router-v1\nv2\ncert\n" ||
  SHA-256(SubjectPublicKeyInfo))` as two hex labels under
  `bind.kmplify-router.invalid`;
* both sides accept only a certificate whose tag the cluster key reproduces
  over the public key in that certificate (no CA, no host name, no clock),
  and rustls checks the handshake signature with that key;
* inside the session the v1 signed request and signed answer still travel
  (defence in depth, and one code path).

**v1 compatibility:** the node-info port answers both. A connection that
starts with a TLS handshake record is v2; anything else is v1, signed plain
HTTP, as before, for nodes before v2 and for apps on this computer. A peer
that announces `v=2` is asked over TLS only, never in plain; a peer that
announces `v=1` (or nothing) in plain. The `peers` entries in `router.json`
carry `proto` and `secure` (its last report came over TLS); the router state
carries `cert_fingerprint` (SHA-256 of this start's certificate).

### Embedding (kmplify-desktop)

An application that embeds the router runs `router::run_with` and acts with
`views::perform_with`, both with its own `router::KeyStore`: kmplify-desktop
keeps the cluster key in its credentials store instead of `router.key`. Key
format (64 hex characters) and fingerprint (the first 4 bytes of SHA-256 over
`kmplify-router-v1/fingerprint/` and the key) are the same, so a desktop and
a node join one cluster with the same key.

### Security model

* **Off by default.** Nothing listens and nothing is announced until the
  operator turns the router on.
* **Loopback by default.** `local` binds 127.0.0.1 only; the bound address is
  checked again after binding.
* **LAN only on explicit opt-in**, and only with an allowlist. Entries must
  be private (10/8, 172.16/12, 192.168/16, fc00::/7), link-local
  (169.254/16, fe80::/10) or CGNAT/VPN (100.64/10) ranges; `0.0.0.0/0` or a
  public address is refused. LAN mode without an allowlist or without a
  cluster key stays local and says why.
* **Dropped before reading.** A connection from an address that may not
  connect is closed without a byte read and without an answer.
* **Every request is authenticated**, loopback included: an HMAC-SHA256
  signature with the cluster key over method, path, time and a random nonce.
  Requests more than 30 seconds off are refused, and a nonce is accepted
  once. The key itself never crosses the network.
* **Every answer is signed back** over the request's nonce and the body, so a
  node accepts figures only from a holder of the key.
* **The key** is 32 random bytes in `router.key` in the node directory,
  owner-only (0600; on Windows an ACL for the current user only), never in
  `settings.json`. Screens and the mDNS record show only an 8-character
  fingerprint.
* **What a node tells the others:** hardware, load, fabric link state, model
  names and job counts. Never credentials, consumers, prompts or log lines.
  A node is named `node-` plus the start of its node id unless you name it;
  the computer's name, which often carries a person's, is not used.
* **Between v2 nodes, mutual TLS** (above): the report is no longer readable
  on the wire. Toward v1 nodes it is signed plain HTTP as before: readable
  on the wire, not forgeable, not obtainable without the key. An attacker
  who forges mDNS records can make a v2 node ask a peer in plain v1; that
  exposes this node's request (no secret) and the peer's signed answer, not
  the key.

## What comes next

1. Phase 2b: routing proxies (:11440 Ollama-, :11441 OpenAI-compatible, one
   endpoint that sends each request to the best placed node) between members
   over the mutual TLS above, and per-GPU telemetry in the report.
2. Remote engine control for paired nodes.
3. The router as an installation admin view in the KMPLIFY web app.

The design adapts NVIDIA's Personal AI Router (Apache-2.0); see NOTICE.
