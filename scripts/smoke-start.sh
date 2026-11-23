#!/usr/bin/env bash
# Start smoke test (Linux, macOS): start `kmplify-node --ui gui` against a
# gateway that does not exist and check that it reaches the dashboard and
# keeps running.
#
#   scripts/smoke-start.sh BINARY VARIANT
#
# VARIANT:
#   plain   the machine as it is
#   hang    a `docker` and an `nvidia-smi` that never answer, first on PATH
#   down    a `docker` whose daemon is not running (Docker Desktop's error)
#
# The fake programs `exec sleep`, so the node killing its child at the time
# limit ends them; nothing here kills by name.
set -u
bin=$1
variant=$2
work=$(mktemp -d)
mkdir -p "$work/fake" "$work/node"
case "$variant" in
  plain) ;;
  hang)
    for t in docker nvidia-smi; do
      printf '#!/bin/sh\nexec sleep 600\n' >"$work/fake/$t"
      chmod +x "$work/fake/$t"
    done
    ;;
  down)
    printf '#!/bin/sh\necho "failed to connect to the docker API at unix:///var/run/docker.sock; check if the path is correct and if the daemon is running: dial unix /var/run/docker.sock: connect: no such file or directory" >&2\nexit 1\n' >"$work/fake/docker"
    chmod +x "$work/fake/docker"
    ;;
  *) echo "unknown variant $variant" >&2; exit 2 ;;
esac

out="$work/out.txt"
started=$(date +%s)
PATH="$work/fake:$PATH" KMPLIFY_NODE_UI_OPEN=0 KMPLIFY_NODE_DIR="$work/node" \
  PROVIDER_GATEWAY_URL=http://127.0.0.1:9 \
  "$bin" run --ui gui </dev/null >"$out" 2>&1 &
pid=$!

fail=0
seen=""
for _ in $(seq 1 90); do
  if grep -q "dashboard on this computer" "$out"; then
    seen=$(($(date +%s) - started))
    break
  fi
  kill -0 "$pid" 2>/dev/null || break
  sleep 1
done
# Alive at 20 s after the start, and 5 s after the dashboard line.
until_alive=$((started + 20))
if [ -n "$seen" ] && [ $((started + seen + 5)) -gt "$until_alive" ]; then
  until_alive=$((started + seen + 5))
fi
while [ "$(date +%s)" -lt "$until_alive" ]; do sleep 1; done
alive=yes
kill -0 "$pid" 2>/dev/null || alive=no

kill -INT "$pid" 2>/dev/null
for _ in $(seq 1 120); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
kill -0 "$pid" 2>/dev/null && kill -9 "$pid"
wait "$pid" 2>/dev/null

echo "=== output ($variant)"
cat "$out"
echo "=== startup.log ($variant)"
cat "$work/node/startup.log" 2>/dev/null || echo "(no startup.log)"
[ -f "$work/node/startup-crash.log" ] && { echo "=== startup-crash.log"; cat "$work/node/startup-crash.log"; }

check() {
  if grep -q "$1" "$out"; then echo "ok: $2"; else echo "FAIL: $2" >&2; fail=1; fi
}
check "starting (node dir" "the first start line"
check "joining" "joining"
check "dashboard on this computer" "the dashboard line"
case "$variant" in
  hang) check "Docker does not answer" "the Docker line" ;;
  down) check "Docker is not running" "the Docker line" ;;
esac
if [ "$alive" = yes ]; then echo "ok: alive at 20 s"; else echo "FAIL: the node had exited" >&2; fail=1; fi
[ -n "$seen" ] && echo "dashboard after ${seen} s"
rm -rf "$work"
exit $fail
