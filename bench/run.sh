#!/usr/bin/env bash
# Build each benchmark natively and interpreted-only, then compare with hyperfine.
# Usage: bench/run.sh [bench/foo.ts ...]   (RUNS=n to change the run count)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
out=build/bench
mkdir -p "$out"
benches=("$@")
[ ${#benches[@]} -eq 0 ] && benches=(bench/*.ts)
for ts in "${benches[@]}"; do
  name=$(basename "$ts" .ts)
  ./target/release/tsaot build "$ts" -o "$out/$name-native"
  ./target/release/tsaot build --no-native "$ts" -o "$out/$name-interp"
  if ! diff <("$out/$name-native" | grep -v elapsed) <("$out/$name-interp" | grep -v elapsed) >/dev/null; then
    echo "$name: native and interpreted outputs differ!" >&2
    exit 1
  fi
  hyperfine -N --warmup 1 --runs "${RUNS:-5}" --export-markdown "$out/$name.md" \
    -n "$name (interpreted)" "$out/$name-interp" \
    -n "$name (native)" "$out/$name-native"
done
