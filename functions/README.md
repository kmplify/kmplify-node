# The fabric's function catalog, in source

These are the WebAssembly modules the KMPLIFY Compute Fabric's gateway signs
and schedules onto provider machines. They live **here**, in the open-source
node's repository, for the same reason the node itself is open: nobody should
be asked to execute bytes they cannot read.

A provider trusts a module because the gateway signed its hash and their node
re-hashes what it downloads. That is a strong guarantee about *provenance* and
none at all about *behaviour* — so behaviour is here, in a few hundred lines
per module, with no dependencies beyond what is listed in each `Cargo.toml`.

## What runs, and what cannot

Each module is a WASI preview-1 **command**: it reads stdin, writes stdout,
exits. Inside the sandbox there is no filesystem, no network, no environment
and no ambient clock — a function cannot phone home, cannot read the
provider's disk and cannot outlive its call. Memory and wall-clock come from
the signed manifest and are clamped again by the operator's own ceilings.

That shapes what is worth writing: deterministic work on the bytes an agent
hands over. Fetching a page is the agent's job (it has a network); turning
900 KB of HTML into the paragraph that matters is this.

| Module | Does | stdin | stdout |
|---|---|---|---|
| `html-to-text` | Strips markup, scripts, styles and entities down to readable text | HTML | text |
| `csv-to-json` | Header row becomes keys, rows become objects (RFC 4180 quoting) | CSV | JSON |
| `json-query` | Selects and reshapes with a small path language (`.items[].name`) | JSON | JSON |
| `hash` | sha256 / sha512 of the input, hex | any bytes | text |

## Build them

```sh
rustup target add wasm32-wasip1
./functions/build.sh            # -> functions/dist/*.wasm, with sha256 for each
```

The profile is size-first (`opt-level = "z"`, LTO, `panic = "abort"`, symbols
stripped) because every provider downloads the bytes once per hash.

## Verify what the fabric serves

The gateway publishes each module's sha256 in its signed manifest, and serves
the bytes it signed:

```sh
curl -s https://fabric.kmplify.io/v1/functions | jq -r '.functions[] | "\(.id) \(.sha256)"'
curl -s https://fabric.kmplify.io/v1/functions/html-to-text/module | shasum -a 256
```

Rebuilding here should reproduce that hash with the same toolchain version;
a mismatch means the catalog is ahead of this source, and the source is what
you should trust less until it is explained.

## Run one locally, in the same sandbox a node uses

```sh
cargo run --example run_function -- functions/dist/html-to-text.wasm < page.html
```

That is the node's own runtime, with the node's own limits — not a
approximation of it.
