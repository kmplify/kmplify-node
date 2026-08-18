# Contributing

Contributions are welcome. Two areas are especially useful:

- **Hardware coverage.** Detection of GPUs, cores, RAM and cgroup limits is
  where this code meets the widest variety of machines and where it is most
  likely to be wrong. A patch that makes a node report an unusual box honestly
  is worth more here than most features.
- **The trust rules.** The pins, clamps and refusals described in the README's
  trust model are the reason this repository is public. Tightening them, or
  showing that one of them does not hold, is the highest-value change you can
  make.

## Before you start

Open an issue for anything larger than a bug fix. The node's behaviour is a
wire contract with gateways that are already deployed, so a change that is
locally sensible can strand running nodes. [PROTOCOL.md](PROTOCOL.md) is the
contract; changes to it need a version note and a story for older peers on both
sides.

## Ground rules for this codebase

- **New nodes must work with old gateways, and old nodes with new gateways.**
  Add fields, do not repurpose them, and make absence mean the previous
  behaviour.
- **Anything the gateway sends is input, not instruction.** Sizes get clamped,
  names get validated, paths get refused. If you add a field that reaches
  `docker`, add the rule that bounds it in the same commit.
- **Fail closed.** An unrecognised template, an unpinned image or a malformed
  frame is refused, never given the benefit of the doubt.
- **Comments explain why.** This codebase leans on comments that record the
  incident behind a decision, because the "obvious" simplification usually is
  the thing that broke last time. Match that.

## Working on it

```bash
cargo test           # 38 tests, no network or hardware needed
cargo clippy --all-targets
cargo fmt
kmplify-node check   # resolves config and probes, connects to nothing
```

CI runs `fmt`, `clippy -D warnings` and `test` on Linux, macOS and Windows.
Keep it green.

Tests should be able to run on a laptop with no GPU and no Docker. The rules
worth testing here (image pins, volume shapes, clamps, telemetry maths) are all
pure functions on purpose. Keep them that way.

## Commits

Sign off every commit:

```bash
git commit -s
```

That line certifies you agree to the [CLA](CLA.md), which lets this project be
shipped inside commercial KMPLIFY products while leaving you every right to
your own work. Please read it; it is short and the reasoning is spelled out.

## What is out of scope

The gateway, the scheduler, billing and the marketplace are not in this
repository and will not be added to it. If your change needs the gateway to do
something new, say so in the issue and it can be picked up on the other side.
