#!/bin/sh
# Build every catalog module for wasm32-wasip1 and report its hash.
#
# The hash is the point: it is what the gateway signs and what a node
# re-computes before it runs anything, so a build that does not print the
# hash has not finished the job.
set -eu
cd "$(dirname "$0")"

# rustup's toolchain first. A distro or Homebrew rustc earlier on PATH has no
# wasm32 standard library of its own, and the failure it produces —
# "can't find crate for `std` ... the target may not be installed" — sends
# people to `rustup target add`, which they have already done.
if [ -x "$HOME/.cargo/bin/cargo" ]; then
  PATH="$HOME/.cargo/bin:$PATH"
  export PATH
fi
cargo build --release --target wasm32-wasip1 -q --manifest-path hash/Cargo.toml 2>/dev/null || {
  echo "cannot build for wasm32-wasip1 with $(command -v cargo)." >&2
  echo "  rustup target add wasm32-wasip1   # and make sure rustup's cargo is the one on PATH" >&2
  exit 1
}
out="$PWD/dist"
mkdir -p "$out"
for dir in */; do
  name="${dir%/}"
  [ -f "$name/Cargo.toml" ] || continue
  echo "building $name"
  (cd "$name" && cargo build --release --target wasm32-wasip1 -q)
  cp "$name/target/wasm32-wasip1/release/$name.wasm" "$out/$name.wasm"
done
echo
echo "module                size      sha256"
for f in "$out"/*.wasm; do
  printf '%-20s  %-8s  %s\n' "$(basename "$f")" \
    "$(wc -c < "$f" | tr -d ' ')" \
    "$(shasum -a 256 "$f" | cut -d' ' -f1)"
done
