#!/usr/bin/env bash
# Build each benchmark interpreted-only and natively with both backends, then
# compare with hyperfine.
# Usage: bench/run.sh [bench/foo.ts ...]   (RUNS=n to change the run count;
# BACKENDS="cranelift" to skip LLVM)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
out=build/bench
mkdir -p "$out"
benches=("$@")
[ ${#benches[@]} -eq 0 ] && benches=(bench/*.ts)
read -r -a backends <<< "${BACKENDS:-cranelift llvm}"
for ts in "${benches[@]}"; do
  name=$(basename "$ts" .ts)
  ./target/release/tsaot build --no-native "$ts" -o "$out/$name-interp"
  cmds=(-n "$name (interpreted)" "$out/$name-interp")
  for b in "${backends[@]}"; do
    ./target/release/tsaot build --backend "$b" "$ts" -o "$out/$name-$b"
    if ! diff <("$out/$name-$b" | grep -v elapsed) <("$out/$name-interp" | grep -v elapsed) >/dev/null; then
      echo "$name: $b and interpreted outputs differ!" >&2
      exit 1
    fi
    cmds+=(-n "$name ($b)" "$out/$name-$b")
  done
  hyperfine -N --warmup 1 --runs "${RUNS:-5}" --export-markdown "$out/$name.md" "${cmds[@]}"
done
