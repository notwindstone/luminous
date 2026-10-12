# typescript-aot

An ahead-of-time compiler for strict TypeScript, built on
[QuickJS-NG](https://github.com/quickjs-ng/quickjs). Functions are compiled to
machine code with [Cranelift](https://cranelift.dev), or with
[LLVM](https://llvm.org) for code that should run as fast as possible
(`--backend llvm`, see [Backends](#backends)):
- numbers, booleans and typed arrays use unboxed machine representations;
- all other values (objects, strings, arrays, closures, class instances,
  promises) are QuickJS values, with exact JS semantics.

Whatever the compiler does not handle (`eval`, constructors of derived
classes, and every function it cannot compile) keeps running in the QuickJS
interpreter, linked into the same executable.

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
             ├─ checker ── typed IR ── SSA IR ── Cranelift or LLVM ── native.o ─┐
             │   (per function; rejects anything it cannot prove)               │
             │                                                                  ├─ cc ─> foo
             └─ rewrite + type erasure ── foo.js ── qjsc ── bytecode + main.c ──┤
                                                                                │
                     generated C glue + runtime ────────────────────────────────┘
```

1. **Selection.** The candidates are:
   - top-level `function` declarations;
   - top-level `const` bindings of arrow functions and function expressions;
   - class methods;
   - module code: each run of top-level statements (loops, `if`s, expression
     statements, `let` / `const` / `var` declarations) becomes a native
     function without parameters, called where the statements were. Class
     declarations, exports and top-level `await` end a run and stay
     interpreted. When the checker rejects a statement, only that statement
     stays interpreted. A module binding that only its run uses is a native
     local; one that other code uses lives in a cell (see 6).

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
3. **Native code.** `crates/tsaot-codegen` lowers the IR to a small
   backend-neutral SSA IR, which Cranelift or LLVM compiles (see
   [Backends](#backends)). Calls between native functions are direct machine
   calls. Exceptions propagate through native frames, freeing their
   temporaries.
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
`patches/0001-tsaot-engine-support.patch`, which adds API and a few
fast paths that keep the engine's semantics:

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
- `JS_NewNativeAsyncGenerator`: the same for async generators. The step
  function also reports awaits; the engine keeps the request queue, awaits,
  and resumes it with the settled value;
- `JS_LAYOUT_*`: the object layout that inline fast paths read (reference
  counts, array elements, typed array storage, string length). It is
  statically asserted in the engine, and again in the generated glue.
- `JS_CachePlainShape` / `JS_NewObjectWithCachedShape` and
  `JS_NewCFunctionDataShaped`: objects and native closures made directly
  with a cached shape;
- promises, as in JSC: `Promise.resolve` / `Promise.reject` of a primitive
  makes a settled promise without resolving functions; resolving a native
  promise whose `constructor` is the intrinsic one (read without running
  code) skips the property lookup; reactions to a settled promise are
  queued without allocating reaction records; `await` of a primitive
  queues its continuation without making a promise
  (`JS_EnqueueAwaitJob`: native frames resume from a job of their own);
- `JS_NewObjectClassData`: an object of a class with its data in the same
  allocation and no property storage (`aot type` objects), and
  `JS_DupObjectShape` for caching their shape;
- `JS_GetOwnDataHint` / `JS_GetGlobalVariableHint`: reads from objects
  that inline caches skip (the global object, built-in constructors) look
  at the index where the property was last found first;
- `JS_ShareObjectShape`: gives the builtin prototypes (`String.prototype`,
  `Array.prototype`, `Map.prototype`, …) shared shapes once their lazily
  created members exist, so that inline caches can hold their methods;
  prototype caches also accept arrays, which are exotic only for their
  indices and `length`;
- `JS_CallBuiltin`: calls a builtin (a C function object) without the
  interpreter's call path, and `JS_Call` (which builtins use to call back:
  `sort` comparators, `map` callbacks) enters native functions' fast
  entries first;
- `JS_ArrayPushFast`: `push`'s own fast path, for native code;
- for native array loops: `JS_ArraySpeciesCreateRoom` (a `map` result with
  room for every element), `JS_TryGetElement`, `JS_DefineElement` (appends
  in place), and `JS_SortNative`, a stable merge sort that calls a native
  comparator directly through its native signature;
- faster engine internals that every program benefits from, interpreted
  or not: 64KB allocator arenas handed out by a bump index, with one empty
  arena kept per size class (allocation-heavy code used to create and free
  an arena every few dozen objects), and recently freed blocks cached per
  size class (a free and the next allocation are a push and a pop); plain
  objects made from an allocation site's shape in one short function, and
  freed (with the plain objects only they referenced) in a loop rather than
  through the collector's queue; arrays freed the same way with their
  plain elements; an empty size-class cache refilled 32 blocks at a time,
  in address order; strings allocated from the cache inline; plain objects
  that keep their property array in their own allocation until it grows;
  defining the next element of a fast array (`map`, `filter`, `split`,
  `Array.from`, …) appends it, and a growing array makes room for 8
  elements at least; `map`,
  `filter`, `forEach`, `some`, `every`, `reduce` and `sort` read the
  elements of fast arrays in place and call a native callback's fast entry
  directly; forward `indexOf` searches by its first character with
  `memchr`; `parseInt`, `Number(s)` and `+s` read small decimal integers
  directly; the cycle collector first runs at 4MB and then whenever the
  heap has doubled, or quadrupled after a collection that freed little
  (rather than from 256KB, at every 50% growth);
  `x ** 2` is the correctly rounded square (as native code computes it).

## Backends

`--backend cranelift` (the default) or `--backend llvm` (or `TSAOT_BACKEND`)
picks the code generator of `tsaot build` / `tsaot run`. Both translate the
same backend-neutral IR (`mir.rs`), which the lowering builds, with the same
semantics: wrapping integer arithmetic, shift amounts modulo the width,
saturating float-to-integer conversions, NaN-propagating `fmin` / `fmax`.
The differential tests run every case with both.

- **Cranelift** compiles in process, in milliseconds. It optimizes little
  beyond what the lowering does itself.
- **LLVM** emits LLVM IR (text) and compiles it with `clang -O3
  -march=native` (clang ≥ 15 on `PATH`, or `--clang` / `TSAOT_CLANG`;
  `--march` picks another CPU, e.g. `x86-64-v3` for portable executables).
  Compiling takes seconds, and the code is optimized much further:
  - the functions that only native code calls are internal to the LLVM
    module, so LLVM inlines them anywhere in the program (recursive `fib` is
    partly inlined into itself), drops the unused ones and picks their
    calling convention;
  - every load and store carries its alias region as type-based alias
    analysis metadata: reference counts, `JSValue` slots (properties,
    elements, cells, frames), the engine's object headers, typed array
    elements, `aot type` storage, inline caches, constant tables. Values
    then stay in registers across stores to other regions (a field is not
    read again after a reference count update), and dead stores go;
  - addresses derived from globals, stack slots and loaded pointers stay
    pointers (`getelementptr`), and the tables filled before any native code
    runs (atoms, strings, shapes) are invariant loads, hoisted out of loops
    and across calls;
  - slow and exception paths become branch weights (laid out of line);
  - libm and the runtime's numeric helpers are declared pure, so their
    results are reused and hoisted;
  - and the usual pipeline applies: GVN, LICM, induction variable
    simplification, unrolling, vectorization where JS semantics allow it
    (the `x`, `y` differences of `bench/nbody_aot.ts` are computed as one
    vector; floating point sums keep their order, so `dot` stays scalar).

  On x86-64, each libm call is followed by an empty `asm` statement that
  marks the vector registers as written: LLVM 18 does not count a call's
  clobbers, and would otherwise convert an integer to a double into a
  register the callee wrote last, a false dependency that serializes
  independent libm calls in a loop (`fill` in `bench/arrays.ts` ran almost
  twice as long).

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
  the same keys, or destructured: `const { x, y } = p`) is never
  allocated: each property becomes a local, an unboxed number when it only
  ever holds numbers. Untyped locals that only ever hold numbers become
  number locals too. Elsewhere, `o.k++`,
  `o.k += n` update a cached data property in place.
- **Control flow**: `if`, `while`, `do`-`while`, `for`, labelled `break` /
  `continue`, `switch`, `try` / `catch` / `finally`, `throw`.
- **JS values**: `let` / `const` / `var` holding anything, plus:
  - string, template, object and array literals;
  - property and element access, assignment and `delete`. Own data
    properties are read and written through per-site inline caches: a shape
    check (one comparison: other kinds of entries keep their shape in a
    second field), then a load or store. A cache also covers the object's own
    getters and setters, methods, getters and setters found on the first
    or second prototype (checking the shapes on the way), and private
    fields and `super.name`. Sites that never cache stop trying;
  - object literals: each site caches the literal's final shape, and
    later objects are made with it directly (as JSC's allocation profiles
    do);
  - `typeof`, `in`, `instanceof`, `new`;
  - calls to any function: native functions directly (small ones without
    loops or closures are inlined, as JIT compilers inline them); a callee only known
    at run time (a method, a getter, a callback) is checked for being a
    native function and then entered through its native entry, skipping
    the engine's call path; interpreted ones through the engine; where
    the code says which native method, getter or setter a call should
    reach (`this` of a class and its superclasses, `super.m()`, members of
    an object literal in a local), a small one is inlined behind a check
    that the function reached is that one (speculative inlining);
  - `-`, `*`, `/`, `%` and `**` between a number and a JS value: the JS
    value is converted inline, and the result is a number (a BigInt
    throws, as mixing it with a number does);
  - arithmetic between JS values (untyped property values, untyped
    locals): numbers take an inline path, and a boxed result carries its
    double along (through joins and in an extra variable of each untyped
    local), so the next operation uses it directly and the boxing goes
    unused unless the value is stored or passed on;
  - globals.

  Unboxed values are boxed where they meet JS values.
- **Arrays and typed arrays**, with inline element access and `.length`.
  Typed array elements are unboxed numbers: `s += a[i] * b[i]` over two
  `Float64Array`s is plain floating-point code. An out-of-bounds read is
  `undefined`, encoded as a reserved NaN inside native code.
- **Closures**: nested functions and arrows, including ones that outlive
  their creator and capture loop variables (one binding per iteration, as in
  JS). A captured variable never assigned after its declaration is
  captured by value: no box is allocated for it. Closures of one function
  share a shape. Side effects on module-level bindings (`count++`) are shared with
  interpreted code.
- **Class methods, getters and setters** (`this` included; static ones
  too), with the class's
  private fields, methods and accessors (`this.#x`, `this.#m()`,
  `#x in obj`) and `super.x` / `super.m()` / `super.x = v`.
- **Class constructors** of base classes whose instances are plain objects
  with public fields (no private names, accessors or computed keys,
  initializers that do not use `this`, a constructor that returns nothing
  but the instance). Native `new C(args)` calls the native constructor
  directly (after reading `C`, which may be in its TDZ): the instance is
  made with the class's fields, initializers evaluated first, through an
  allocation site that caches the instance's final shape, then the body
  runs. A constructor that only assigns its fields (`this.x = x`, values
  that run no code) makes the instance with them directly, so an inlined
  `new V(a, b)` is an object expression. Interpreted code keeps the
  class's own constructor.
- **Class instances that do not escape** (scalar replacement, as JIT
  compilers' escape analysis): a local holding a new instance (`new V()`,
  or the result of an inlined or predicted method that makes one, such as
  `a.sub(b)`) and only used for its fields, its class's methods and as an
  argument of predicted methods keeps its fields in locals of its own while
  a flag says it is virtual. Field reads and writes use them; its methods
  run inline over them behind a check that the class's prototype still has
  the method; a method of another object that receives it reads them
  through its parameter. Where the object itself is needed (a method that
  changed, a prediction that fails, a call that is not inlined) it is made
  then ("materialized", as deoptimization does), and later uses work on
  it, so identity and mutations stay right.
- **Predictions from type annotations**: where an annotation names a
  top-level class (`o: Vec3`, `xs: Sphere[]`, a field `center: Vec3`, a
  method or function returning `Vec3`, `Node | null`), calls on those
  values are predicted to reach that class's native methods, as for
  `this`: a small method is inlined behind a check that the method reached
  is that one. Annotations are never trusted otherwise: a wrong one only
  costs the check.
- **Fields of annotated object types**: where an annotation names an
  object type alias, an interface or a class with plain public fields
  (`b: Body`, `bs: Body[]`, a field `pos: Body`), its fields are read and
  written at fixed slots. The first object of the type seen with exactly
  the declared fields, in order, as writable data properties gives the
  type's shape; each access then compares the object's shape with it (one
  load and one comparison, shared by all the type's sites) and loads or
  stores the slot. Any other object (fields in another order, missing or
  extra ones, accessors, frozen) goes through the site's inline cache.
- **Builtins**: method calls on strings read `String.prototype`'s methods
  through their inline caches; `s.charCodeAt(i)` runs inline and
  `arr.push(x)` takes the engine's fast path while they are the original
  builtins (checked at each call); `===`, `!==` and `switch` against
  string constants compare pointers, then lengths, before characters; a
  direct call with a JS value for a `number` or `boolean` parameter checks
  it at the call instead of going through the callee's JS entry. Where the
  arguments cannot run code, the method is read without taking a
  reference.
- **Array callbacks as native loops**: `xs.forEach(cb)`, `map`, `filter`,
  `some`, `every`, `find` and `findIndex` with an arrow function written at
  the call become a loop with the arrow's body inline (its captured
  variables are the caller's own), as JIT compilers inline the builtin and
  its callback; it runs while `xs` is an array without holes and the
  method is the original builtin, reading each element afresh (the
  callback may change the array), and calls the builtin otherwise.
  `xs.sort((a, b) => ...)` with an arrow that captures nothing has the
  engine sort calling the compiled comparator directly.
- **Leaf functions** that can neither throw nor call (only unboxed numbers
  and booleans) skip the stack check, and their callers the exception
  check after the call.
- **`async` functions and arrows**: `await` anywhere in an expression,
  including `finally` blocks and loop updates. Awaits are hoisted into
  statements while keeping JS evaluation order, so microtask ordering
  matches the interpreter exactly. `await Promise.resolve(x)` awaits `x`
  itself while `Promise.resolve` is the intrinsic one (the same ticks).
- **Generators** (`function*`, generator methods and closures), with
  `yield`, `yield*` and the exact `next` / `return` / `throw` behavior:
  native generator objects are the engine's own.
- **Async generators** (`async function*`, methods and closures) and
  **`for await`**, with the interpreter's exact await points: `yield v`
  awaits `v`, `return v` and `return(v)` await `v` before `finally` blocks
  run, and `yield*` awaits each inner result. Requests queue up as in the
  engine, which drives native async generator objects as its own.
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
- **Object literal methods, getters and setters** (`this` included;
  generator and async methods too; `super` in them
  is not supported yet).

Not yet: `super()` and the constructors of derived classes or of classes
with private names (which stay interpreted), computed method and accessor names in object
literals, classes inside functions, `arguments`, and `this` in arrow functions. A
function using one of these stays interpreted (and functions that call it
call it through its binding). `tsaot report file.ts` says, for every
function, whether it is native and, if not, which construct stopped it.

### Forcing native compilation: `aot` (or `compiled`)

Unmarked functions are compiled on a best-effort basis. A function marked
`aot` **must** compile natively, or the build fails with the reason.
`compiled` is another name for `aot`, everywhere it applies (`compiled
function`, `compiled (x) => ...`, `"use compiled"`, `compiled type`):

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

A nested function can be marked when its enclosing function (or module
code) is compiled, so `xs.map(aot (x) => …)` works at the top level too.
Functions in class constructors and field initializers cannot be marked:
interpreted code constructing the class runs them interpreted.

### Keeping a function interpreted: `interpreted`

The opposite keyword keeps a function in the interpreter, with the same
syntax (`interpreted function f() {}`, `interpreted (x) => …`). The directive
form is `"use no aot"`, which also works on class methods. A native function
cannot contain an interpreted one, so marking a nested function keeps its
enclosing function interpreted as well (a build error if that one is marked
`aot`). Native code calls an interpreted function through its binding, as it
calls any function that is not native.

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

## The `int` type

`type int = number` declares an integer type that the compiler checks:
an `int` variable, parameter or return value always holds an int32 (never
`-0`). TypeScript sees a plain `number`, so the program runs unchanged with
`tsc`, Bun or Node; tsaot keeps `int`s in machine registers.

```ts
type int = number;

function isPrime(n: int): boolean {
  if (n < 2) return false;
  if (n % 2 === 0) return n === 2;
  for (let d: int = 3; d * d <= n; d = (d + 2) | 0) {
    if (n % d === 0) return false;
  }
  return true;
}
```

- **Range wraps, kind is checked.** Every store into an `int` (initializer,
  `=`, `+=` / `-=` / `*=` / `>>>=`, `++` / `--`, `return` from a function
  annotated `int`, parameter default) must store an integer:
  - an int32 is stored as is: an int32 literal, another `int`, a bitwise
    result (`|`, `&`, `^`, `~`, `<<`, `>>`), `Math.imul`, `Math.clz32`,
    `Math.min` / `max` of ints, a call to a function returning `int`;
  - an integer that may not fit (`+`, `-`, `*` of integers, `>>>`) wraps
    like `| 0`: `count++`, `count += n` and `count = count + n` all store
    `(count + n) | 0`;
  - anything else is a compile error: fractions (`count = count + 1.5`,
    `count += 0.5`), `/`, `%`, `**`, most `Math` functions, values of
    unknown type (`xs[i]`; write `xs[i] | 0`), out-of-range literals.

  The check covers the whole module, native or not, and the interpreted
  half of the program is rewritten to wrap the same stores, so native and
  interpreted code always agree. Under Bun or Node, which ignore the
  annotation, results differ only once a value overflows. Wrapping happens
  at the store: within an expression, `+` and `*` are JS arithmetic
  (`(a * b * c) | 0` is what JS computes, not a 32-bit multiply).

  With `--no-int-wrapping`, integers that may not fit are compile errors
  instead, except for a `for` counter stepped by one under an int bound
  (`i < n` with `n` an `int`).
- **Checked at the boundary.** Interpreted code can pass anything. A native
  function whose `int` argument is not an int32 (`1.5`, `"1"`, `-0`,
  `2 ** 31`) declines the call, and the interpreted version runs instead,
  with ordinary JS semantics.
- Arithmetic on `int`s is exact: `a + b` and `a * b` are computed in 64-bit
  integers, and the double result is rounded only where JS would round it.
  So comparisons such as `d * d <= n` and `%` (`-4 % 2` is `-0`, `x % 0` is
  `NaN`) need no floating point.
- `type double = number` is accepted as a plain alias of `number`. An `int`
  captured by a closure or declared at module level is checked and wrapped
  like any other; native code keeps it as a JS value.

## `aot type`

`aot type` declares an object type with a fixed layout:

```ts
aot type Point = {
  x: int; y: int; z: int;
  kind: "colored";
  color: string;
} | {
  x: int; y: int; z: int;
  kind: "uncolored";
};

aot type AB = { a: number } | { b: string };   // { a }, { b } and { a, b }
aot type XY = { x: int } & { y: number };        // one member: { x, y }

const p: Point = { x: 0, y: 0, z: 0, kind: "colored", color: "#fff" };
```

- **Members and fields.** An `aot type` is object types combined with `|`
  and `&` (and other `aot type`s), at the top level of the module. Fields
  are `int`, `number` / `double`, `boolean`, `string`, string literals or
  another `aot type`; no optional fields, methods, index signatures or
  type parameters.
- **One layout.** All the members' fields share one layout, with a mask of
  the fields an object has. An object is valid when it has every field of
  some member, with that member's types, and its other fields belong to
  some member (so `{ a } | { b }` holds `{ a }`, `{ b }` and `{ a, b }`).
  Fields that every member stores the same way are unboxed: an `int` is an
  int32, a `number` a double, a `boolean` a byte.
- **Made by literals.** An object literal where an `aot type` is expected
  makes one: the initializer of a binding declared with it, a value
  assigned to one or to such a field, a returned value, an argument for
  such a parameter, an element of an array declared `T[]` (in an array
  literal, `a[i] = ...` or `a.push(...)`). Its fields are checked at
  compile time: unknown, missing or ill-typed fields are errors. The same
  literals in interpreted code make the same objects.
- **Kept to its type.** To JS, an object of an `aot type` is a plain-looking
  object (`JSON.stringify`, `Object.keys`, spread, `in` all work) that is
  not extensible: fields cannot be added, deleted or redefined, and a field
  only takes values of its type, else a TypeError, from native or
  interpreted code alike. An `int` field wraps other integers to int32 (as
  `| 0`; with `--no-int-wrapping`, a TypeError); a write that would leave
  the object matching no member is a TypeError. `Object.freeze` throws.
- **Arrays of them.** An array literal or `new Array(n)` (or `Array(n)`)
  where `T[]` / `Array<T>` is expected makes an array that only holds
  objects of `T` (or holes): storing anything else in it is a TypeError,
  while to JS it is still an array (`Array.isArray`, JSON, spread and the
  `Array.prototype` methods work; `push`, `unshift`, `splice` and `fill`
  arguments that are literals make objects of `T`). Native code reads an
  element as an object of `T` or `undefined` (reading a field of
  `undefined` throws as in JS), and so field reads through it are typed.
  A `let` / `const` declared `T[]` that is only ever assigned such arrays
  is typed everywhere, module bindings included.
- **Native code** reads and writes fields at fixed offsets: on a value of a
  declared `aot type` (a literal, a parameter, a field), `p.x` is one load
  of an int32 and arithmetic on it stays in integers. Elsewhere (an element
  of a plain array), property caches recognize the object's layout. An
  object that does not escape is not allocated at all: its fields become
  locals, with the same checks on stores. In a loop that runs no other code
  (no calls, getters or conversions), the fields it uses live in registers
  and are stored back after the loop. A native function whose `aot
  type` parameter gets another value declines the call (the interpreted
  version runs). A call whose argument of an `aot type` is not known
  to be one (an element of an untyped array) checks it at the call and
  calls the native function directly.

## Getting started

Requires Nix with flakes (or rustc ≥ 1.88, clang, cmake and ninja on `PATH`).
The LLVM backend runs clang (≥ 15) at compile time; the Cranelift backend
needs only a C compiler, for linking.

```sh
git clone --recursive https://github.com/notwindstone/typescript-aot
cd typescript-aot
nix develop
scripts/build-quickjs.sh          # patches/ applied to vendor/quickjs, built into build/quickjs
cargo build --release

./target/release/tsaot run bench/fib.ts
./target/release/tsaot build bench/mandelbrot.ts -o mandelbrot
./target/release/tsaot build --backend llvm bench/nbody_aot.ts -o nbody
./target/release/tsaot report tests/cases/rejected.ts
./target/release/tsaot emit-js bench/fib.ts       # what the interpreter runs
./target/release/tsaot ir bench/fib.ts            # typed IR of native functions
cargo test                                        # differential tests (leak-checking engine), both backends
bench/run.sh                                      # hyperfine: interpreted vs Cranelift vs LLVM
```

`scripts/build-quickjs.sh` also builds `build/quickjs-check`, an engine with
assertions enabled, which the tests link against. The compiled executable
targets the host CPU (like `-march=native`).

## Benchmarks

Mean wall time for the whole program, measured with hyperfine. "Interpreted" is
the same program built with `--no-native`, i.e. plain QuickJS-NG bytecode.

| benchmark    | what it stresses                                   | interpreted | native  | speedup |
|--------------|----------------------------------------------------|------------:|--------:|--------:|
| mandelbrot   | float loops                                        |      932 ms |   35 ms |   26.4× |
| xorshift     | bitwise ops (`^`, `<<`, `>>`, `>>>`)               |     3.69 s  |  242 ms |   15.2× |
| fib          | recursive calls                                    |      506 ms |   36 ms |   13.9× |
| integrate    | libm calls (`sin`, `exp`, `sqrt`)                  |     4.13 s  |  414 ms |   10.0× |
| arrays       | `Uint8Array` sieve, `Float64Array` dot, `number[]` |     3.77 s  |  629 ms |    6.0× |
| primes       | `%`, loops                                         |     4.37 s  |  925 ms |    4.7× |
| boundary     | 5M tiny native calls *from interpreted code*       |      636 ms |  375 ms |    1.7× |

In `boundary`, the interpreted loop around the calls dominates.

## Roadmap

- **`int` fields and elements**: `{ value: int }`, `int[]` (checked reads),
  and `int` parameters of closures.
- Shapes for class instances (inline property access), and calls to methods
  of known classes without the engine.
- **Multi-module programs** (`import` between `.ts` files).
- **Position-preserving type stripping**, so interpreter stack traces point
  at the original `.ts` lines.

## Layout

```
crates/tsaot-front    oxc front end, checker, typed IR, type erasure, `aot` marker
crates/tsaot-codegen  lowering to a backend-neutral SSA IR (mir.rs); Cranelift (clif.rs) and LLVM (llvm.rs) backends
crates/tsaot-cli      `tsaot` binary: glue generation, qjsc + clang + cc driver; differential tests
runtime/              C runtime linked into every program (helpers, boxes, async frames, cells)
vendor/quickjs        QuickJS-NG (git submodule, kept pristine)
patches/              engine patch applied by scripts/build-quickjs.sh
tests/cases           differential test programs
tests/errors          programs that must fail to compile, with expected diagnostics
bench/                benchmarks + bench/run.sh; bench/micro/ for microbenchmarks
```
