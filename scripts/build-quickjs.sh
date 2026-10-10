#!/usr/bin/env bash
# Build QuickJS-NG with tsaot's engine patches into build/quickjs.
#
# vendor/quickjs stays pristine: its tree is exported to build/quickjs-src,
# patches/*.patch are applied there, and that copy is built.
set -euo pipefail
cd "$(dirname "$0")/.."
git submodule update --init vendor/quickjs
rm -rf build/quickjs-src
mkdir -p build/quickjs-src
git -C vendor/quickjs archive HEAD | tar -x -C build/quickjs-src
for p in patches/*.patch; do
  echo "applying $p"
  patch -d build/quickjs-src -p1 --quiet < "$p"
done
cmake -S build/quickjs-src -B build/quickjs -G Ninja -DCMAKE_BUILD_TYPE=Release
ninja -C build/quickjs qjsc qjs qjs_exe
