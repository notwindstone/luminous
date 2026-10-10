# typescript-aot

An ahead-of-time compiler for strict TypeScript, built on
[QuickJS-NG](https://github.com/quickjs-ng/quickjs). Functions whose types
fully determine their behaviour are compiled to machine code with
[Cranelift](https://cranelift.dev). Everything else (dynamic code, objects,
strings, `eval`, ...) keeps running in the QuickJS interpreter, linked into the
same executable.

```console
$ tsaot report bench/primes.ts
bench/primes.ts:1: native       isPrime
bench/primes.ts:10: native       countPrimes
$ tsaot build bench/primes.ts -o primes && ./primes
primes below 2e6: 148933
elapsed: 905 ms         # 4.5x faster than the same program interpreted
```

## How it works

```
             ┌──────────── oxc parser + semantic analysis
 foo.ts ─────┤
             ├─ strict-subset checker ── typed IR ── Cranelift ── native.o ─────┐
             │   (per function; rejects anything it cannot prove)               │
             │                                                                  ├─ cc ─> foo
             └─ rewrite + type erasure ── foo.js ── qjsc ── bytecode + main.c ──┤
                                                                                │
                     generated C glue (entry stubs) + runtime ──────────────────┘
```

1. **Selection.** Every top-level `function` declaration, and every top-level
   `const` bound to an arrow function or function expression, is a candidate. The
   checker (`crates/tsaot-front/src/check.rs`) lowers it to a typed IR only if
   every construct in its body has exact, statically known JS semantics.
   Otherwise it records why and leaves the function to the interpreter.
   A fixpoint removes functions that call non-native ones.
2. **Native code.** `crates/tsaot-codegen` lowers the IR with Cranelift to
   functions over unboxed `double`s and `bool`s. Calls between native functions
   are direct machine calls.
3. **Interpreted code.** The whole module is type-erased (oxc transformer) and
   compiled by `qjsc` to bytecode. Each native declaration `f` is renamed to a
   fallback `__tsaot_fb_f` and its binding is re-pointed at its entry stub;
   native `const` initializers are wrapped in place:
   ```js
   import { __tsaot_bind } from "tsaot:native";
   const f = __tsaot_bind(0, __tsaot_fb_f);    // first statement of the module
   const g = __tsaot_bind(1, (x) => x * 2);    // `const g = (x: number) => x * 2`
   ```
4. **Entry stubs (the "deopt" point).** TypeScript types are unsound at
   runtime: interpreted code can pass a string where `number` is declared. Each
   stub checks the argument tags. On a match it unboxes and calls native code;
   on a mismatch it calls the interpreted fallback, which has exactly the
   original JS semantics. Native code therefore never sees a value outside its
   declared type.

## The native subset (phase 1)

- Parameters annotated `number` or `boolean`; return type `number`, `boolean`,
  `void`, or inferred (as `tsc` would, without recursion through unannotated
  functions).
- `let` / `const` locals with initializers, assignment and compound assignment,
  `++` / `--`.
- All arithmetic, comparison, bitwise, logical (`&&`, `||` with JS truthiness),
  conditional and comma operators on numbers and booleans.
- `if`, `while`, `do`/`while`, `for`, labelled `break` / `continue`, `return`.
- Calls to other native functions; `Math.*` (all pure functions and constants),
  `Number.isNaN` / `isFinite` / `isInteger` / `isSafeInteger`, `Number.*`
  constants, `NaN`, `Infinity`.
- Module-level `const`s with literal values, folded at compile time.

`tsaot report file.ts` says, for every function, whether it is native and, if
not, which construct stopped it.

### Forcing native compilation: `aot`

Unmarked functions are compiled on a best-effort basis. A function marked
`aot` **must** compile natively, or the build fails with the reason:

```ts
aot function fib(n: number): number { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
const sq = aot (x: number): number => x * x;
const cube = aot function (x: number): number { return x * x * x; };

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
        `helper` is interpreted: property access is not supported
```

`aot` is a contextual keyword, like `async`. It marks a function only when it
directly precedes one on the same line (`aot function`, `aot async function`,
`aot (…) => …`). Everywhere else `aot` is an ordinary
identifier: `aot(x)` is still a call. The keyword is not TypeScript syntax,
so `tsc`, editors and formatters reject it. Use `"use aot"` where that matters.
For now, `aot` can mark top-level function declarations and top-level `const`
function bindings. On async functions, generators, methods or nested
functions it is a compile error that says so.

### Semantics and soundness

The goal is to be **observably identical to the interpreter**. The
differential test suite (`tests/cases`) builds every case both natively and
interpreted-only and requires byte-identical output. It covers `-0`, NaN,
ToInt32 at every boundary, `%`, `**`, `Math.round` half-cases, `Math.min`/`max`
with `-0`/NaN, fallbacks on ill-typed calls, and stack overflow.

- Functions are compiled only if their binding is never reassigned. Direct
  `eval` anywhere in the module disables native compilation for the module,
  because it could rebind anything.
- Reading a block-scoped variable before its declaration (TDZ) is rejected
  statically.
- Native recursion is bounded. Stack overflow unwinds the native frames and
  throws the same `RangeError` the interpreter would. Native frames are much
  smaller than interpreter frames, so native code can recurse deeper before
  hitting the limit. Stack depth is implementation-defined in ECMAScript.
- **`Math` and `Number` are locked.** Before any JS runs (in native and
  `--no-native` builds alike), every own property of `Math`, the
  `Number.is*` predicates, and the global `Math` / `Number` bindings are made
  non-writable and non-configurable. Patching them throws a `TypeError`, and
  patches visible in the source (`Math.sqrt = …`, `delete Number.isNaN`,
  `Object.defineProperty(Math, …)`) are compile errors. New properties can
  still be added.
- A top-level `const` function declared *after* other top-level code may
  still be in its TDZ when native code runs. It is compiled, but other
  native functions do not call it directly.

Known observable differences:
- `f.toString()` of a native function returns native code instead of the
  source.
- A native function is not a constructor (`new f()` throws).

## Getting started

Requires Nix with flakes (or rustc ≥ 1.88, clang, cmake and ninja on `PATH`).

```sh
git clone --recursive https://github.com/notwindstone/typescript-aot
cd typescript-aot
nix develop
scripts/build-quickjs.sh          # builds vendor/quickjs into build/quickjs
cargo build --release

./target/release/tsaot run bench/fib.ts
./target/release/tsaot build bench/mandelbrot.ts -o mandelbrot
./target/release/tsaot report tests/cases/rejected.ts
./target/release/tsaot emit-js bench/fib.ts       # what the interpreter runs
./target/release/tsaot ir bench/fib.ts            # typed IR of native functions
cargo test                                        # differential tests
bench/run.sh                                      # hyperfine: native vs interpreted
```

The compiled executable targets the host CPU (like `-march=native`).

## Benchmarks

Mean wall time for the whole program, measured with hyperfine. "Interpreted" is
the same program built with `--no-native`, i.e. plain QuickJS-NG bytecode.

| benchmark    | what it stresses                         | interpreted | native  | speedup |
|--------------|------------------------------------------|------------:|--------:|--------:|
| mandelbrot   | float loops                              |      884 ms |   35 ms |   25.5× |
| fib          | recursive calls                          |      427 ms |   32 ms |   13.3× |
| integrate    | libm calls (`sin`, `exp`, `sqrt`)        |     3.90 s  |  584 ms |    6.7× |
| primes       | `%`, loops                               |     4.09 s  |  0.91 s |    4.5× |
| xorshift     | bitwise ops (`^`, `<<`, `>>`, `>>>`)     |     3.59 s  |  1.12 s |    3.2× |
| boundary     | 5M tiny native calls *from interpreted code* | 524 ms  |  420 ms |    1.25× |

`boundary` is the worst case: the cost is dominated by QuickJS's
C-function call path, not by native code.

## Roadmap

- **Int32 representation selection**: keep integer-valued locals in machine
  integers (loop counters, bitwise code). This is the biggest remaining win
  for `primes` and `xorshift`.
- **Objects and arrays**: `Float64Array` / `number[]` with bounds-checked
  native access, then class instances with known shapes, through the QuickJS
  C API (safe because QuickJS uses refcounting and does not move objects).
- **Strings**, and calls from native code back into interpreted code (with
  exception propagation through the existing unwind mechanism).
- **Multi-module programs** (`import` between `.ts` files).
- **Position-preserving type stripping**, so interpreter stack traces point
  at the original `.ts` lines.
- Optional full strict type check with `tsc --strict --noEmit` (available in
  the dev shell).

## Layout

```
crates/tsaot-front    oxc front end, strict-subset checker, typed IR, type erasure
crates/tsaot-codegen  Cranelift backend (IR -> object file)
crates/tsaot-cli      `tsaot` binary: glue generation, qjsc + cc driver; differential tests
runtime/              C runtime linked into every program (pow, round, ToInt32, stack guard)
vendor/quickjs        QuickJS-NG (git submodule)
tests/cases           differential test programs
tests/errors          programs that must fail to compile, with expected diagnostics
bench/                benchmarks + bench/run.sh
```
