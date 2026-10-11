# typescript-aot

An ahead-of-time compiler for strict TypeScript, built on
[QuickJS-NG](https://github.com/quickjs-ng/quickjs). Functions are compiled to
machine code with [Cranelift](https://cranelift.dev):
- numbers, booleans and typed arrays use unboxed machine representations;
- all other values (objects, strings, arrays, closures, class instances,
  promises) are QuickJS values, with exact JS semantics.

Whatever the compiler does not handle (`eval`, async generators, class
constructors, and every function it cannot compile) keeps running in the
QuickJS interpreter, linked into the same executable.

```console
$ tsaot report bench/primes.ts
bench/primes.ts:1: native       isPrime
bench/primes.ts:10: native       countPrimes
$ tsaot build bench/primes.ts -o primes && ./primes
primes below 2e6: 148933
elapsed: 905 ms         # 4.4x faster than the same program interpreted
```

## How it works

```
             ┌──────────── oxc parser + semantic analysis
 foo.ts ─────┤
             ├─ checker ── typed IR ── Cranelift ── native.o ───────────────────┐
             │   (per function; rejects anything it cannot prove)               │
             │                                                                  ├─ cc ─> foo
             └─ rewrite + type erasure ── foo.js ── qjsc ── bytecode + main.c ──┤
                                                                                │
                     generated C glue + runtime ────────────────────────────────┘
```

1. **Selection.** The candidates are:
   - top-level `function` declarations;
   - top-level `const` bindings of arrow functions and function expressions;
   - class methods.

   The checker (`crates/tsaot-front/src/check.rs`) lowers each one to a typed
   IR, together with the functions nested in it, which become native closures.
   It does this only if every construct has exact, known JS semantics; otherwise
   it records why and leaves the function to the interpreter.
2. **Types.** TypeScript types are hints, not facts. A value is unboxed only
   where its type is proven:
   - parameters annotated `number`, `boolean` or a typed array are checked on
     entry;
   - locals are typed by their initializers (with a retry that widens a local
     to a JS value when it is later assigned something else);
   - typed arrays also come from `new Float64Array(…)`.

   Everything else is a `JSValue`, and operations on it go through inline fast
   paths and then the engine's own implementation.
3. **Native code.** `crates/tsaot-codegen` lowers the IR with Cranelift. Calls
   between native functions are direct machine calls. Exceptions propagate
   through native frames, freeing their temporaries.
4. **Interpreted code.** The whole module is type-erased (oxc transformer) and
   compiled by `qjsc` to bytecode. Each native function's binding is
   re-pointed at a function object whose entry is native code. The original
   function is kept as its *fallback*:
   ```js
   import { __tsaot_bind, __tsaot_method, ... } from "tsaot:native";
   const f = __tsaot_bind(0, __tsaot_fb_f);    // first statement of the module
   const g = __tsaot_bind(1, (x) => x * 2);    // `const g = (x: number) => x * 2`
   class P { norm() { … } };__tsaot_method(2, P, 0, "norm");
   ```
5. **JS entries (the "deopt" point).** Interpreted code can pass a string
   where `number` is declared. Each native function has a generated entry that
   checks its typed arguments. On a match it runs native code. On a mismatch it
   declines, and the engine calls the interpreted fallback, which has exactly
   the original semantics.
6. **Shared state.** Module-level bindings that native code uses live in
   *cells*, which both sides access. Interpreted references to reassigned
   bindings are rewritten to go through an accessor object (`__tsaot_m.count`).
   Variables captured by native closures live in GC-visible boxes. Every
   reference in native code is counted exactly like the interpreter's. The
   test suite runs on an engine built with assertions, which aborts if any
   object leaks.

### Crossing from interpreted to native code

Calls from the interpreter into native code are cheap. The whole boundary is
machine code:

- **An engine hook.** The engine patch adds `JSCFunctionFast`, an optional
  fast entry on C function objects. The interpreter's `OP_call` /
  `OP_call_method` call it directly on their stack slots, without recursing
  into `JS_CallInternal` and without pushing a stack frame. Other callers
  (`f.call`, `Array.prototype.map`, …) reach it from the generic
  C-function path.
- **Inline guard and unboxing.** `tsaot_fast_<i>` reads the argument tags and
  payloads straight from the interpreter's `JSValue` slots.
- **Inlined body.** Unless a function is large, its body is compiled straight
  into its JS entry.
- **Result in registers.** The `JSValue` comes back in `rax:rdx` (`x0:x1` on
  AArch64). Writing it to memory as two halves and reloading it as one 16-byte
  value would defeat store forwarding. That stall used to be the largest cost
  of a call.

`bench/micro/crossing.ts` measures one call from an interpreted loop:

| callee | interpreted callee | native callee |
|---|---:|---:|
| `id(x)` (pure crossing) | 38 ns | **22 ns** |
| `lerp(a, b, t)` (cost beyond doing the same math inline) | 36 ns | **6 ns** |

QuickJS-NG's interpreter loop is very sensitive to code layout. In A/B runs,
even a never-taken branch in `OP_call` moved unrelated benchmarks by −23% to
+9%. The hook is therefore an out-of-line `unlikely` jump that leaves the
regular call path's generated code alone.

### The engine patch

`vendor/quickjs` stays pristine. `scripts/build-quickjs.sh` applies
`patches/0001-tsaot-engine-support.patch`, which only *adds* API:

- `JSCFunctionFast` and `JS_NewCFunctionDataFast`, the call hook above;
- `JS_BinaryOperator` / `JS_UnaryOperator`: every JS operator, with exactly
  the interpreter's semantics, for the slow paths of native code;
- `JS_GetGlobalVariable` / `JS_SetGlobalVariable`: strict-mode global access;
- `JS_AwaitValue`: resume a native async function when a value settles;
- `JS_CacheOwnProperty` and friends: inline caches. A cache holds a reference
  to a shape, which makes the engine clone that shape instead of modifying
  it in place, so every object with the shape has the property in the same
  slot;
- the interpreter's own iteration (`for`-`of`, `for`-`in`, spread,
  destructuring), CopyDataProperties (object spread and rest), property
  access with a receiver (`super`), private field access and brand checks,
  and reading a private name's value from a closure;
- `JS_ForInGet`, `JS_ArrayAppendFast`: `for`-`in` over an object's shape
  and in-place array growth (`xs[xs.length] = v`);
- `JS_NewNativeGenerator`: generator objects whose body is native code,
  driven by the engine's own `next` / `return` / `throw`, and resumed
  directly by native `for`-`of` (`JS_NativeGeneratorNext`);
- `JS_LAYOUT_*`: the object layout that inline fast paths read (reference
  counts, array elements, typed array storage, string length). It is
  statically asserted in the engine, and again in the generated glue.

## What compiles natively

- **Numbers and booleans**, unboxed: all operators, `Math.*`, the
  `Number.is*` predicates and constants, and module-level literal `const`s
  (folded).
- **Integer locals**: a `number` local only ever assigned integer literals
  and stepped by small integers (`i++`, `n += 2`) lives in a machine
  integer, exact past 2^53 (it rounds as doubles do there). Comparisons,
  array indexes and bitwise operators use the integer directly, and the
  range check disappears when loop trip counts bound the value statically.
- **Objects that do not escape**: a local initialized with an object
  literal and only used as `local.key` (and reassigned only literals with
  the same keys) is never allocated: each property becomes a local, an
  unboxed number when it only ever holds numbers. Elsewhere, `o.k++`,
  `o.k += n` update a cached data property in place.
- **Control flow**: `if`, `while`, `do`-`while`, `for`, labelled `break` /
  `continue`, `switch`, `try` / `catch` / `finally`, `throw`.
- **JS values**: `let` / `const` / `var` holding anything, plus:
  - string, template, object and array literals;
  - property and element access, assignment and `delete`. Own data
    properties are read and written through per-site inline caches: a shape
    check, then a load or store;
  - `typeof`, `in`, `instanceof`, `new`;
  - calls to any function: native functions directly, interpreted ones
    through the engine;
  - globals.

  Unboxed values are boxed where they meet JS values.
- **Arrays and typed arrays**, with inline element access and `.length`.
  Typed array elements are unboxed numbers: `s += a[i] * b[i]` over two
  `Float64Array`s is plain floating-point code. An out-of-bounds read is
  `undefined`, encoded as a reserved NaN inside native code.
- **Closures**: nested functions and arrows, including ones that outlive
  their creator and capture loop variables (one binding per iteration, as in
  JS). Side effects on module-level bindings (`count++`) are shared with
  interpreted code.
- **Class methods** (`this` included; static methods too), with the class's
  private fields, methods and accessors (`this.#x`, `this.#m()`,
  `#x in obj`) and `super.x` / `super.m()` / `super.x = v`.
- **`async` functions and arrows**: `await` anywhere in an expression,
  including `finally` blocks and loop updates. Awaits are hoisted into
  statements while keeping JS evaluation order, so microtask ordering
  matches the interpreter exactly.
- **Generators** (`function*`, generator methods and closures), with
  `yield`, `yield*` and the exact `next` / `return` / `throw` behavior:
  native generator objects are the engine's own.
- **Iteration**: `for`-`of` and `for`-`in`; iterators are closed on `break`,
  `return` and exceptions as in JS. Arrays are iterated directly while array
  iteration is the original one, and native generators are resumed directly
  while `%GeneratorPrototype%.next` is the original one (both checked when a
  loop starts). `for`-`in` walks the object's shape when it can (no index
  keys, nothing enumerable on the prototype chain), and `obj[k]` with the
  loop key reads the slot directly while the shape is unchanged.
- **Destructuring** in declarations, parameters, `catch`, loop heads and
  assignments; **spread** in calls, `new`, and array and object literals;
  **rest** parameters and elements; **optional chaining**; computed keys in
  object literals.

Not yet: async generators and `for await`, `super()` and class constructors
(which stay interpreted), getters, setters and methods in object literals,
classes inside functions, `arguments`, and `this` in arrow functions. A
function using one of these stays interpreted (and functions that call it
call it through its binding). `tsaot report file.ts` says, for every
function, whether it is native and, if not, which construct stopped it.

### Forcing native compilation: `aot`

Unmarked functions are compiled on a best-effort basis. A function marked
`aot` **must** compile natively, or the build fails with the reason:

```ts
let count: number = 0;
aot function withSideEffects() { count++; }

aot function debounced(delay: number, callback: () => void): () => void {
  let timeoutId: number;
  return aot (): void => {
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => callback(), delay);
  };
}

// Equivalent directive form, which keeps the file valid TypeScript for tsc,
// editors and formatters:
function clamp(x: number, lo: number, hi: number): number {
  "use aot";
  return Math.min(Math.max(x, lo), hi);
}
```

```
  × `aot` function `top` cannot be compiled natively
   ╭─[example.ts:6:46]
 6 │ aot function top(x: number): number { return mid(x) * 2; }
   ·                                              ───┬──
   ·                                                 ╰── calls `mid`, which is not compiled natively
   ╰────
  help: `mid` is interpreted: calls `helper`, which is not compiled natively
        `helper` is interpreted: unsupported statement: debugger
```

`aot` is a contextual keyword, like `async`. It marks a function only when it
directly precedes one on the same line (`aot function`, `aot async function`,
`aot (…) => …`). Everywhere else `aot` is an ordinary identifier: `aot(x)` is
still a call. The keyword is not TypeScript syntax, so `tsc`, editors and
formatters reject it. Use `"use aot"` where that matters.

A nested function can be marked when its enclosing function is compiled. A
callback written in top-level code (`xs.map(aot (x) => …)`) cannot be marked
yet. `"use no aot"` keeps a function interpreted.

### Semantics and soundness

The goal is to be **observably identical to the interpreter**. The
differential test suite (`tests/cases`) builds every case natively and
interpreted-only and requires byte-identical output. It covers, among
others:
- `-0`, NaN, ToInt32 at every boundary;
- typed array conversions, detached and resizable buffers;
- array holes and accessors;
- exceptions through native frames;
- async interleavings and generator resumption (`return` / `throw` while
  suspended in `try` / `finally`, `yield*` forwarding);
- iterator closing on every kind of exit, and evaluation order in
  destructuring;
- private brand checks and `super` with receivers.

- Functions are compiled only if their binding is never reassigned. Direct
  `eval` anywhere in the module disables native compilation for the module,
  because it could rebind anything.
- Native recursion is bounded. Stack overflow unwinds the native frames and
  throws the same `RangeError` the interpreter would.
- **Some builtins are locked.** Before any JS runs (in native and
  `--no-native` builds alike), the following are made non-writable and
  non-configurable:
  - every own property of `Math`;
  - the `Number.is*` predicates;
  - the global `Math`, `Number` and typed array constructor bindings.

  Patching them throws a `TypeError`, and patches visible in the source are
  compile errors. Each typed array prototype also gets the intrinsic `length`
  getter as a locked own property. With that, the `length` of a typed array
  with no own properties and its original prototype is always its element
  count, which native code reads inline.

Known observable differences:
- `f.toString()` of a native function returns native code instead of the
  source.
- A native function is not a constructor (`new f()` throws).
- Native frames do not appear in `Error.stack`.
- `Object.getOwnPropertyNames(Float64Array.prototype)` includes `length` (see
  above).
- A class whose native methods use its private names or `super` gets a
  static block first in its body (handing them to native code), which shows
  in the class's `toString()`.

## Getting started

Requires Nix with flakes (or rustc ≥ 1.88, clang, cmake and ninja on `PATH`).

```sh
git clone --recursive https://github.com/notwindstone/typescript-aot
cd typescript-aot
nix develop
scripts/build-quickjs.sh          # patches/ applied to vendor/quickjs, built into build/quickjs
cargo build --release

./target/release/tsaot run bench/fib.ts
./target/release/tsaot build bench/mandelbrot.ts -o mandelbrot
./target/release/tsaot report tests/cases/rejected.ts
./target/release/tsaot emit-js bench/fib.ts       # what the interpreter runs
./target/release/tsaot ir bench/fib.ts            # typed IR of native functions
cargo test                                        # differential tests (leak-checking engine)
bench/run.sh                                      # hyperfine: native vs interpreted
```

`scripts/build-quickjs.sh` also builds `build/quickjs-check`, an engine with
assertions enabled, which the tests link against. The compiled executable
targets the host CPU (like `-march=native`).

## Benchmarks

Mean wall time for the whole program, measured with hyperfine. "Interpreted" is
the same program built with `--no-native`, i.e. plain QuickJS-NG bytecode.

| benchmark    | what it stresses                                   | interpreted | native  | speedup |
|--------------|----------------------------------------------------|------------:|--------:|--------:|
| mandelbrot   | float loops                                        |      858 ms |   35 ms |   24.4× |
| fib          | recursive calls                                    |      415 ms |   35 ms |   12.0× |
| integrate    | libm calls (`sin`, `exp`, `sqrt`)                  |     4.13 s  |  476 ms |    8.7× |
| arrays       | `Uint8Array` sieve, `Float64Array` dot, `number[]` |     3.74 s  |  669 ms |    5.6× |
| primes       | `%`, loops                                         |     3.97 s  |  911 ms |    4.4× |
| xorshift     | bitwise ops (`^`, `<<`, `>>`, `>>>`)               |     3.53 s  |  1.12 s |    3.2× |
| boundary     | 5M tiny native calls *from interpreted code*       |      510 ms |  359 ms |    1.4× |

In `boundary`, the interpreted loop around the calls dominates.

## Roadmap

- **Int32 representation selection** beyond counters and steps: integer
  arithmetic in general (`%`, bitwise code, parameters). This is the biggest
  remaining win for `primes` and `xorshift`.
- Shapes for class instances (inline property access), and calls to methods
  of known classes without the engine.
- **Multi-module programs** (`import` between `.ts` files).
- **Position-preserving type stripping**, so interpreter stack traces point
  at the original `.ts` lines.

## Layout

```
crates/tsaot-front    oxc front end, checker, typed IR, type erasure, `aot` marker
crates/tsaot-codegen  Cranelift backend (IR -> object file)
crates/tsaot-cli      `tsaot` binary: glue generation, qjsc + cc driver; differential tests
runtime/              C runtime linked into every program (helpers, boxes, async frames, cells)
vendor/quickjs        QuickJS-NG (git submodule, kept pristine)
patches/              engine patch applied by scripts/build-quickjs.sh
tests/cases           differential test programs
tests/errors          programs that must fail to compile, with expected diagnostics
bench/                benchmarks + bench/run.sh; bench/micro/ for microbenchmarks
```
