#!/usr/bin/env bash
# Build the vendored QuickJS-NG (qjsc + static libraries) into build/quickjs.
set -euo pipefail
cd "$(dirname "$0")/.."
git submodule update --init vendor/quickjs
cmake -S vendor/quickjs -B build/quickjs -G Ninja -DCMAKE_BUILD_TYPE=Release
ninja -C build/quickjs qjsc qjs qjs_exe
