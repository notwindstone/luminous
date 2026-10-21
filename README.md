# Luminous

Luminous is an ahead-of-time compiler for strict TypeScript, built on
[QuickJS-NG](https://github.com/quickjs-ng/quickjs). Functions are compiled to
machine code with [Cranelift](https://cranelift.dev), or with
[LLVM](https://llvm.org) for code that should run as fast as possible
(`--backend llvm`, see [Backends](#backends)):
- numbers, booleans and typed arrays use unboxed machine representations;
- all other values (objects, strings, arrays, closures, class instances,
  promises) are QuickJS values, with exact JS semantics.

Whatever the compiler does not handle (`eval`, classes inside functions,
and every function it cannot compile) keeps running in the QuickJS
interpreter, linked into the same executable.

Luminous is the compiler only. Beyond, the runtime built on it (web-interoperable
WinterTC APIs, a build API, and so on), is a separate project.

```console
$ luminous report bench/primes.ts
bench/primes.ts:1: native       isPrime
bench/primes.ts:10: native       countPrimes
$ luminous build bench/primes.ts -o primes && ./primes
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
   - top-level `const` bindings of arrow functions and function expressions,
     and `let` and `var` ones never assigned or declared again (the
     functions of `var a = function a() {...}, b = ...`, as Bun's modules
     declare theirs);
   - class methods;
   - module code: each run of top-level statements (loops, `if`s, expression
     statements, `let` / `const` / `var` declarations) becomes a native
     function without parameters, called where the statements were. Class
     declarations (and declarations of class expressions not compiled as
     classes), exports and top-level `await` end a run and stay
     interpreted. When the checker rejects a statement, only that statement
     stays interpreted. A module binding that only its run uses is a native
     local; one that other code uses lives in a cell (see 6).

   The checker (`crates/luminous-front/src/check.rs`) lowers each one to a typed
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
3. **Native code.** `crates/luminous-codegen` lowers the IR to a small
   backend-neutral SSA IR, which Cranelift or LLVM compiles (see
   [Backends](#backends)). Calls between native functions are direct machine
   calls. Exceptions propagate through native frames, freeing their
   temporaries. What a loop body declares (`const`, `let`) is released when
   its iteration ends, as the interpreter does, so two iterations' data are
   never alive at once.
4. **Interpreted code.** The whole module is type-erased (oxc transformer) and
   compiled by `qjsc` to bytecode. Each native function's binding is
   re-pointed at a function object whose entry is native code. The original
   function is kept as its *fallback*:
   ```js
   import { __lum_bind, __lum_method, ... } from "luminous:native";
   const f = __lum_bind(0, __lum_fb_f);    // first statement of the module
   const g = __lum_bind(1, (x) => x * 2);    // `const g = (x: number) => x * 2`
   class P { norm() { … } };__lum_method(2, P, 0, "norm");
   ```
5. **JS entries (the "deopt" point).** Interpreted code can pass a string
   where `number` is declared. Each native function has a generated entry that
   checks its typed arguments. On a match it runs native code. On a mismatch it
   declines, and the engine calls the interpreted fallback, which has exactly
   the original semantics.
6. **Shared state.** Module-level bindings that native code uses live in
   *cells*, which both sides access. Interpreted references to reassigned
   bindings are rewritten to go through an accessor object (`__lum_m.count`).
   Variables captured by native closures live in GC-visible boxes. Native
   code takes and drops references for free: the engine's collector traces
   what is reachable, native frames included (see
   [Memory](#memory-a-generational-tracing-collector)).

### Crossing from interpreted to native code

Calls from the interpreter into native code are cheap. The whole boundary is
machine code:

- **An engine hook.** The engine patch adds `JSCFunctionFast`, an optional
  fast entry on C function objects. The interpreter's `OP_call` /
  `OP_call_method` call it directly on their stack slots, without recursing
  into `JS_CallInternal` and without pushing a stack frame. Other callers
  (`f.call`, `Array.prototype.map`, …) reach it from the generic
  C-function path.
- **Inline guard and unboxing.** `lum_fast_<i>` reads the argument tags and
  payloads straight from the interpreter's `JSValue` slots.
- **Inlined body.** Unless a function is large, its body is compiled straight
  into its JS entry.
- **Result in a register.** The `JSValue` (8 bytes, see
  [Values](#values-8-bytes)) comes back in `rax` (`x0` on AArch64), and
  never touches memory on its way back into the interpreter.

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
`patches/*.patch` in order (`0001-luminous-engine-support.patch`, then
`0002-fast-paths.patch`, `0003-promises-and-object-props.patch`,
`0004-prototype-epoch.patch`, `0005-json-writers.patch`,
`0006-native-construction.patch`, `0007-promises.patch`,
`0008-function-source.patch`, `0009-arguments.patch`,
`0010-class-constructors.patch`, `0011-compressed-pointers.patch`,
`0012-pristine-builtins.patch`, `0013-json-trailing-holes.patch`,
`0014-gc-marking.patch`, `0015-object-model.patch`,
`0016-gc-trigger-inline.patch`, `0017-allocation-fast-paths.patch`,
`0018-await-or-value.patch`, `0019-json-fast-paths.patch`,
`0020-exotic-define-own.patch`, `0021-latin1-strings.patch`,
`0022-count-own-keys.patch`, `0023-frozen-fast-arrays.patch` and
`0024-json-parse-and-for-in.patch`,
`0025-objects-and-dictionaries.patch`, `0026-string-search-and-adds.patch`,
`0027-for-await-close.patch`, `0028-large-cells-and-strings.patch`,
`0029-stack-overflow-message.patch`, `0030-function-classes.patch`,
`0031-bound-fast-calls.patch`, `0032-promise-thenable-fast.patch`,
`0033-symbol-key-fast-paths.patch`,
`0034-typed-array-proto-getters.patch`, `0035-symbol-atom.patch` and
`0036-proxy-own-shape.patch`, each made
against the tree the earlier ones give) and builds the engine without
the stack protector, semantic interposition and CET markers distribution
compilers default to (it is linked statically: its exported functions
inline into each other), and without straight-line vectorization. The patch adds API and a few fast paths that keep the
engine's semantics:

- `JSCFunctionFast` and `JS_NewCFunctionDataFast`, the call hook above;
- `JS_BinaryOperator` / `JS_UnaryOperator`: every JS operator, with exactly
  the interpreter's semantics, for the slow paths of native code;
- `JS_GetGlobalVariable` / `JS_SetGlobalVariable`: strict-mode global access;
- `JS_AwaitValue`: resume a native async function when a value settles;
- `JS_CacheOwnProperty` and friends: inline caches. A cache holds a reference
  to a shape, which makes the engine clone that shape instead of modifying
  it in place, so every object with the shape has the property in the same
  slot;
- `JS_WatchPrototype` and `js_proto_epoch` (patch 0004): a watched object
  (a prototype whose method or getter a cache holds) moves a global counter
  whenever it changes: a property added, deleted, redefined or written,
  its prototype set. Watching gives it a shape of its own once (and its
  writes are never cached), so no native store bypasses the counter;
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
- `JS_LAYOUT_*`: the object layout that inline fast paths read (cell
  headers, array elements, typed array storage, string length). It is
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
  `JS_NewPendingPromise` / `JS_SettlePromise`: a promise without resolving
  functions, settled once by native code as its resolve or reject function
  would (thenables followed), which is what a native async function's
  promise is; resolving functions are made with a cached shape that has
  their `length` and `name` (they were defined one at a time, two
  property definitions each) (`0003`);
- `JS_NewObjectClassData`: an object of a class with its data in the same
  allocation and no property storage (`aot type` objects), and
  `JS_DupObjectShape` for caching their shape;
- `JS_PrivateDefine` and `JS_PrivateAddBrand`: what a class's
  construction does to an instance (a private field defined, the brand of
  its private methods given), for native constructors; and a native
  closure's data (`JS_NewCFunctionDataShaped`) in the function object's
  own cell, after its properties, rather than a separate allocation its
  finalization freed, made of a given prototype, and
  `JS_CFunctionDataTemplate`, what native code makes the next ones
  inline from; a new object's flags, class id and weak index set in one
  store (they were set a bit field at a time, or zeroed then set)
  (`0006`);
- `JS_SetFunctionSourceHook`: what `Function.prototype.toString` gives for a
  C function data object, which the generated glue answers for native
  functions (`0008`, see below), and `JS_FunctionToString`;
- `JS_NewArguments`: the (unmapped) `arguments` object of a strict
  function called with given arguments, which the JS entries of native
  functions using `arguments` make (`0009`);
- 8-byte values with compressed references (`0011`, see
  [Values](#values-8-bytes));
- builtins that skip what the specification looks up while it is still the
  builtins' own, as JSC's (`0012`). The bits of what the promise builtins
  rely on (re-checked when the prototype epoch moves: no cost while
  nothing changes) also cover %RegExp.prototype% (`exec`, `flags` and the
  flag getters, the symbol methods), array species
  (`Array.prototype.constructor`, `Array[Symbol.species]`) and the absence
  of `then` on %Object.prototype%. While they hold, and for a regexp of
  the original shape: `match`, `search` and `replace` of strings call the
  regexp builtins directly, which read the flags from the compiled regexp
  and call `exec` without looking it up; `exec` reads and writes
  `lastIndex` in its slot, keeps the captures on the stack and appends to
  its result array directly (one `str.match(re)` in a Hono request: 7,300
  instructions, now 2,700). `slice` of an array of the original shape
  copies its elements, and `ArraySpeciesCreate` is an array (Hono's two
  slices: 3,800 instructions, now 2,100, all but allocation). `await` of
  an object with no `then` on its prototype chain, and a promise resolved
  with one, is fulfilled without looking it up (no promise made for the
  await). `Object.keys` of a plain object of a shared shape comes from a
  cache of the keys of its shape (900 instructions, now 380);
- `JSON.stringify` of an array whose length is more than its elements
  (holes at the end) by the generic path, which reads them from the
  prototypes (`0013`: the fast path wrote only the elements);
- what a mark function marks for (`JS_GCMarkKind`: a minor collection,
  a major one) and whether a cell is old (`JS_IsOldCell`), so that the
  runtime marks only the inline caches that hold young shapes in a minor
  collection; and open variable references marked by the functions and
  `arguments` objects holding them, as the closed ones are: a function made
  old while one of its variables was still open lost it, young, to a minor
  collection once the variable closed (`0014`);
- caches for the object model, as JSC's (`0015`): `JS_CacheInstanceOf`
  (`instanceof` by the constructor's shape and its `prototype` slot,
  then a walk of the chain), `JS_CacheAddProperty` and
  `JS_AddCachedProperty` (an addition as a shape transition, with the
  prototypes watched), `JS_CacheAbsentProperty` (a property that objects
  of a shape and their prototypes lack); an array literal's elements in
  the array's own cell (`JS_NewArrayFrom`, `slice`, `exec`'s results);
  and an array's elements and an object's properties, once out of its
  cell, in a cell of their own that it marks rather than storage its
  finalization frees (arrays need no finalization, up to 63 elements);
- the collection trigger's test inline in allocations, its work out of
  line (`0016`);
- plain objects (`JS_NewObjectProtoClass`, `Object.create`) and arrays of
  the original shape (literals, `new Array`, `slice`, empty ones) made
  inline, as `JS_NewObjectClassData`'s objects are: the header in one
  store, none of the general path's per-class work (an array literal:
  178 instructions, now 117, collections included);
  `JS_NewArrayWithRoom`: an empty array with room in its own cell, which
  native code makes for `[]` (4 elements: the pushes that follow need no
  storage of their own) (`0017`);
- `JS_SetCFunctionDataClassConstructor`: a C function data object as a
  class constructor, for classes inside functions: `new` calls it with
  `this` new.target, a call without `new` throws, and it has no fast entry
  (which calls, native or the interpreter's, would take) (`0010`);
- promises and jobs without allocations of their own, as JSC's (`0007`):
  the job queue is a ring of entries (a job was an allocation, freed once
  run); a promise's data is in its own cell; a pending promise's
  reactions are GC cells in one list for either outcome (two records in
  two lists, allocated and freed, the promise finalized for them); the
  data of resolving functions is in their own cells, the reject function
  holding the resolve function whose flag both share (three allocations
  per pair, two finalizers); `JS_AwaitNative`: an await whose reaction
  queues the native frame's resumption directly (no resolve and reject
  functions, no call through the engine); `JS_AwaitNativeOrValue`
  (`0018`): the value an await continues with at once, when suspending
  could not be told apart (see async functions below);
  `JS_NewPromiseSettledWith`:
  the promise of an async function that completed without suspending;
  `JS_NewObjectClassDataShaped`: frames allocated with their shape known.
  Fast paths, taken while the builtins they stand for are the original
  ones (`Promise.prototype.then` and `constructor`, `Promise[@@species]`,
  `Promise.resolve`, array iteration: their objects are watched, so a
  change to any of them is seen at once) and no promise hook is set:
  `then` of a promise of `%Promise%` makes the derived promise without
  resolving functions (the reaction settles it as they would);
  `Promise.all`, `allSettled`, `any` and `race` over an array of promises
  of `%Promise%` and other values react to each natively (no element
  functions, no derived promises, no iterator objects). Microtask order is
  the spec's: the result of each is reacted to in the same job, at the
  same place in the queue;
- `JS_GetOwnDataHint` / `JS_GetGlobalVariableHint`: reads from objects
  that inline caches skip (the global object, objects of shapes of their
  own) look at the index where the property was last found first;
- `JS_CountOwnKeys` (`0022`): how many keys `Object.keys` would give an
  ordinary object or a fast array, without making them: an array's
  elements, then the enumerable string-keyed properties of the shape,
  counted once per shared shape (kept on it);
- frozen fast arrays (`0023`): `Object.freeze` of a fast array keeps its
  elements in place (it made them ordinary properties, one at a time, and
  every read then looked them up: a loop over 1,000 frozen objects, 43–61
  ns an element, now 13–15 ns, as an unfrozen array; bun 44–53 ns). A flag
  on the array sends every write the engine would do in place (an element
  store, `pop`, `shift`, `reverse`, `sort`, `copyWithin`, the interpreter's
  own element stores) to the general path, which sees the elements as
  read-only and non-configurable, and native code stores in place only to
  an extensible array;
- `JSON.parse` from the text's bytes in place (`0024`: a string's own
  Latin-1 characters, else its UTF-8), the values made directly: numbers
  exactly without `strtod` where they can be (int32s as such: it made every
  number a double), strings scanned 8 bytes at a time, and objects made
  with the shape of the last object whose first key was the same, or of
  the array element before, as V8 follows transitions: each key compared
  with the shape's (no atom looked up) and the values stored in its slots.
  Anything else (an error, a reviver, deep nesting) runs the general
  parser, so that the values and errors are its own. A 6-field object with
  a nested one: 13,013 instructions, now 3,053 (1,470 ns, now 308–311 ns;
  bun 360–363 ns); `JS_ParseJSONUTF8Fast` parses bytes that need not end
  with a NUL (a body). The reviver's parse records were not traced by the
  collector: an object only they held (a value a duplicate key replaced)
  was freed while they still referred to it; a mark hook now marks them;
- for-in by shape (`0024`): the keys of a shape enumerated by it, with
  their slots, are an iterator of the shape's, shared, cached by the
  context while its prototypes, watched, are unchanged; a loop's iterator
  is in its own cell (it was a separate allocation, freed by a finalizer),
  and native code steps a shape's shared one itself (`JS_ForInStart`):
  `for (const k in o) n++` over 5 keys, 1,512 instructions, now 293
  (144–152 ns, now 25–30 ns; bun 11–22 ns);
- objects and dictionaries (`0025`): a shape transition found in the
  shape table was checked property by property (an object made of 64
  keys compared about 2,000 pairs); a shape now remembers the one it is
  one property more than, with that one's number (each shape gets a new
  number whenever it enters the table, and forgets when it leaves it), so
  a known transition is one comparison. `obj[key]` with a key that is an
  atom's own string (a literal, or any key used before: its atom index is
  kept in the string, where there was padding) reads and writes an
  ordinary object's data properties without the general lookup. A 64-key
  dictionary built and read with dynamic keys: about 515 instructions a
  key, now about 225 (bun's takes two thirds of the time). The shape an addition
  cached at a write site leads to is pinned, so that compiled code gives
  it to objects without counting them; an empty object made by compiled
  code (`{}`, `Object.create`) has room in its cell for 4 properties,
  which an object's properties grow into before moving out
  (`JS_ShapeAllocInfoRoom`, `JS_NewObjectWithCachedShapeRoom`);
- string searches and sums (`0026`): `lastIndexOf` searched by comparing
  the needle at every position; it now finds its first character with
  `memrchr` (and has a direct entry, `JS_CallStringFn`), as `indexOf`
  does with `memchr`: on a path of 29 characters, 36 ns, now 16 ns (bun
  6–10 ns). `a + "/" + b` of values not known to be strings is one call,
  `JS_AddValuesN`, which concatenates them at once when they are strings
  and primitives (else adds them in turn, as `+` does): 56 ns, now 38 ns
  (bun 26–31 ns);
- `for await` closing its iterator on an exception (`0027`): the body's
  exception closed the iterator from the interpreter's unwinding, which
  called `return` without awaiting its result, so the exception went on
  (to a `catch` after the loop, say) before the iterator was closed (a
  directory of `fs.opendir` was still open there). The body now has a
  handler of its own, as a `using` scope has, which calls `return`,
  awaits its result and ignores what either throws (the spec's
  AsyncIteratorClose of a throw completion), then rethrows; natively
  compiled `for await` loops do the same;
- large cells and strings made in place (`0028`): a cell larger than
  16 KB has a chunk of its own, mapped when it is made and unmapped when
  it dies, so a loop reading a 1 MB file into a buffer or a string paid
  for an `mmap`, page faults over the whole megabyte and a `munmap` each
  time. Up to 64 MB of such chunks (each at most 16 MB) are now kept when
  they die, and a cell of the same size, or up to a quarter smaller,
  takes one back. `JS_NewStringUninit` makes a string whose characters
  the caller writes (a file read straight into it) and
  `JS_ShrinkNewString` gives back what it did not use;
- the message of a stack overflow (`0029`): `JS_SetStackOverflowMessage`
  sets what the `RangeError` says (an embedder copying another engine's
  wording; the default is `Maximum call stack size exceeded`);
- the last function class (`0030`): `JS_LAYOUT_CLASS_GENERATOR_FUNCTION`,
  for native `typeof` (the classes from `JS_LAYOUT_CLASS_C_FUNCTION` to
  it are the callable ones below the proxy's);
- bound functions called from native code (`0031`): `JS_CallFast` of a
  function bound without arguments (`f.bind(x)`, as Bun's `node:fs`
  exports its binding's functions) whose target is a natively compiled
  function enters the target on the bound `this`, where it went through
  `JS_CallInternal` and `js_call_bound_function` (copying the arguments)
  to `JS_Call` the target;
- a promise resolved with a promise (`0032`): an async function returning
  one, `resolve(p)`. The spec's job then calls `p.then` with new
  resolving functions, which makes a derived promise; when `then` is the
  builtin and `p` a promise of `%Promise%` (its constructor and species
  unchanged), none of that is observable, and a native reaction settles
  the promise where the resolving function would be called (the same
  jobs, in the same order);
- symbol keys (`0033`): `obj[sym]` and `obj[sym] = v` take the fast path
  of keys that are atoms' strings (an ordinary object's data property)
  where they took the general lookup (Bun's `this[kFd]`);
- typed arrays' prototype properties (`0034`): a typed array is exotic
  for its numeric keys only, so the caches take its other properties
  (they skipped every exotic object but arrays); a getter up to three
  prototypes away (`byteLength` of a Buffer, a subclass of Uint8Array)
  is found for a polymorphic entry; `%TypedArray%.prototype`'s `length`
  and `byteLength` getters are read directly when they are the ones
  cached (`JS_TypedArrayGetterValue`); and a prototype watched gets a
  shared shape if it had none (a builtin's, once its lazily made members
  are), which is what the caches can take;
- a symbol's atom (`0035`, `JS_SymbolAtom`), without a reference: for
  the caches of computed member sites (`obj[sym]`);
- a proxy has a shape of its own, not hashed (`0036`): it had the one
  `Object.create(null)` gives, and a cache checks an object's shape
  alone, so a proxy read what a site had cached for such objects (a
  property they lack: `undefined`) without asking its handler;
- `instanceof` without calling `Symbol.hasInstance` when the constructor
  cannot have its own (none on its prototype chain up to
  `Function.prototype`, whose is fixed): OrdinaryHasInstance directly
  (`0002`);
- `JSON.stringify` without a replacer or indentation writes plain data
  (objects of class `Object` with data properties, no `toJSON` and no index
  keys, dense arrays, strings, numbers, booleans, `null`) straight from
  shapes and arrays; anything else runs the general algorithm from the
  start (`0002`). It writes into a buffer the runtime keeps between calls
  (a string grown as it was written was reallocated about ten times for a
  small object), and makes one string of the result's length; strings are
  scanned for characters to escape 8 bytes at a time; numbers are written
  without making strings: integers two digits at a time, and decimals of
  up to 8 fraction digits as the shortest that reads back exactly (checked
  with one division per digit count; others go to the engine's `js_dtoa`);
  ropes (strings built by `+`) are written too (`0005`). A hashed shape
  (one shared by its objects, which a change unhashes first) keeps what
  JSON writes of its objects' properties, `,"key":` for each, made the
  first time: an object of it is written key by key from that, each key
  and number, short string, boolean or `null` in room reserved once, with
  no per-key checks (atom kind, index keys, flags, escapes). Dates are
  written as `toISOString` writes them, without calling `toJSON`, while
  `Date.prototype`'s `toJSON`, `toISOString`, `valueOf` and
  `@@toPrimitive` are the builtins (`toISOString` itself no longer goes
  through `printf`), and whether `Object.prototype` and `Array.prototype`
  have a `toJSON` is a watched bit rather than a lookup per object
  (`0019`). A 4-property object (`{ id, name, x, y }`): 2,035
  instructions, now 788 (bun 1.4.3: 1,311; Bun's AOT compiler, untyped:
  1,293); with two Dates: 23,551, now 1,429 (bun 12,524; Bun's AOT
  compiler 1,687);
- `JS_SetJSONWriter` and `JS_JSONWrite8` / `Int` / `Number` / `Value`: the
  same fast path hands an object of a registered class (no properties in
  its shape; `Object.prototype`, `null` or `Array.prototype` as its
  prototype, without `toJSON`) to the class's writer, which writes it from
  its own data or declines (the general algorithm then runs). `aot type`
  objects and arrays of them are written this way (`0005`);
  `JS_JSONStringifyData` / `JS_JSONStringifyWith`: the fast path called
  directly, the latter with a function writing a record of such an object;
  `JS_JSONRoom` / `JS_JSONCommit` and the `JS_JSONPut*` writers: room in
  the output for code outside the engine, which writes into it directly
  (`0019`);
- `JS_ShareObjectShape`: gives the builtin prototypes (`String.prototype`,
  `Array.prototype`, `Map.prototype`, …), constructors and namespaces
  (`Object`, `Array`, `JSON`, `Reflect`, …) shared shapes once their
  lazily created members exist, so that inline caches can hold their
  methods (Elysia's `Object.create`, `JSON.stringify`: 1,000 instructions
  a request);
  prototype caches also accept arrays, which are exotic only for their
  indices and `length`;
- `JS_CallBuiltin`: calls a builtin (a C function object) without the
  interpreter's call path, and `JS_Call` (which builtins use to call back:
  `sort` comparators, `map` callbacks) enters native functions' fast
  entries first;
- `JS_ArrayPushFast`: `push`'s own fast path, for native code;
- for native array loops: `JS_ArraySpeciesCreateRoom` (a `map` result with
  room for every element), `JS_TryGetElement`, `JS_DefineElement` (appends
  in place), and `JS_SortNative`, which sorts a fast array's elements
  with a merge sort compiled with the native comparator;
- faster engine internals that every program benefits from, interpreted
  or not: 64KB allocator arenas handed out by a bump index, with one empty
  arena kept per size class (allocation-heavy code used to create and free
  an arena every few dozen objects), and recently freed blocks cached per
  size class, up to 64MB each (a free and the next allocation are a push
  and a pop); plain objects made from an allocation site's shape in one
  short function; the property array of an object of any class (a
  function, an array, a promise, a typed array) in the object's own
  allocation until it grows, as a plain object's (it was a separate
  allocation, freed by the object's finalization) (`0003`); an empty size-class
  cache refilled 32 blocks at a time,
  in address order; strings allocated from the cache inline, in a path
  that makes no calls (so it saves no registers); pieces of 8-bit strings
  up to 32 bytes (`slice`, `substring`, `split`) copied with a few
  overlapping moves into a cell taken inline; plain objects
  that keep their property array in their own allocation until it grows;
  defining the next element of a fast array (`map`, `filter`, `split`,
  `Array.from`, …) appends it, and a growing array makes room for 8
  elements at least; `map`,
  `filter`, `forEach`, `some`, `every`, `reduce` and `sort` read the
  elements of fast arrays in place and call a native callback's fast entry
  directly; forward `indexOf` searches by its first character with
  `memchr` (8-bit strings: only where a match can start), and native
  code's `indexOf`, `includes`, `startsWith` and `endsWith` with a string
  argument skip the conversions; 8-bit strings of up to 16 characters
  compared for equality (`===`, `Map` keys) with a few word loads, and
  native code compares a string with a short constant (`switch` cases,
  `=== "user"`) by length, then by its characters as immediates; a
  string that is not an atom keeps its hash once a `Map` or `Set` has
  computed it, and an atom's is the hash it has as an atom (`0003`); `Map` and `Set` keys are hashed with a final mix (small
  integers and pointers spread over the buckets) and each entry keeps its
  key's hash, compared before the key; native code calls
  `Map.prototype.get`, `set`, `has` and `Set.prototype.add`, `has` on a
  Map or Set directly while they are the builtins, and `new Set(array)` /
  `new Map(entries)` take an array's elements without its iterator while
  the array and its iteration are the builtin ones; `+` on a string and a
  string or an int32 concatenates directly (an int32's digits written
  into the result), and `s += x` onto a string longer than 8KB appends
  into a buffer with room for as much again, of which the strings are
  slices (one that is no longer the end of its buffer is copied when
  appended to), rather than making a rope; a regular expression search whose pattern starts with
  a character (not under `i` or `y`) tries the pattern only where
  `memchr` finds that character, and a global `replace` by a string
  without `$` on a standard RegExp loops over the matches without
  building their result arrays; global variables
  (`parseInt`, `String`) are read through an inline cache of the global
  object's property (while there are no global `let`/`const`
  definitions); `parseInt`, `Number(s)` and `+s` read small decimal integers
  directly; `x ** 2` is the correctly rounded square (as native code
  computes it);
- `console.info` and `console.debug` (to stdout, as `console.log`),
  `console.warn` and `console.error` (to stderr), as Node's and Bun's.

### Memory: a generational, tracing collector

The patch replaces the engine's reference counting and cycle collector.
Values are not counted, so taking or dropping a reference costs nothing,
in native code and in the engine; a mark-sweep collector frees what is
unreachable, and never moves anything.

- **Heap.** Objects, strings, ropes, bigints, shapes, function bytecode,
  closure variables and async function states are *cells* in 256KB chunks,
  one cell size per chunk, with a bit per cell for allocated, marked and
  to-finalize. Allocation bumps a pointer through a run of free cells,
  counted allocated as a whole when the run begins (which is where a
  collection starts); native code allocates objects inline the same way,
  a compare, a bump and the header stores. QuickJS's list of GC objects
  is gone, and with it 16 bytes of every object; a plain object's
  properties start where other classes keep their own part, and the
  weak references to an object are listed off the runtime (the object
  keeps a 32-bit index in what was padding): a plain object is 24 bytes
  and its properties (8 bytes each, see [Values](#values-8-bytes)), and
  `{ left, right }` with its 8-byte cell header takes 48 bytes (it took
  112). Memory traffic is
  what allocation-heavy code waits on: 32 more bytes per object cost a
  binary-trees run 14%. A chunk's pages are made resident as allocation
  runs reach them (all of a run's at once, rather than a fault each), so
  that the chunks of the cell sizes a small program barely uses stay
  mostly unmapped: hello world peaks at 5.6MB (it was 11.5MB with whole
  regions populated up front).
- **Tracing.** A cell is marked by its chunk's bit alone, without reading
  it (the bit also tells an old cell from a young one); its header and
  body are read when it is traced, a few cells later, prefetched in the
  meantime, as marking waits on memory more than anything.
- **Roots.** The contexts, the job queue, the pending exception, the atoms
  referenced as atoms, mark hooks for values that C structures hold (the
  native runtime's state and inline caches, libc's timers: a minor
  collection marks only the caches given a young shape since the last
  one, which are listed, rather than all of them), and the native
  stack and registers, scanned conservatively: a word pointing into a cell
  keeps it. Native code needs no stack maps: what its registers and stack
  slots hold is found.
- **Generations.** A cell that survives two minor collections is old (a
  bit in its header) and keeps its mark. The first time, it stays young:
  an age bit in its chunk records it, and its mark is cleared after the
  sweep, so that what dies shortly after a collection (a tree being
  built, an array being mapped) dies young instead of filling the old
  generation until a major collection. A minor collection traces from the
  roots and from the old cells given references since the last
  collection, which a write barrier (an inline test of that bit, in the
  engine and in native code) remembers; the old cells are kept without
  being traced. An old cell that still holds young ones after a minor
  collection stays remembered. The young generation starts at 4MB and
  grows, up to 32MB, while much of it survives; a major collection traces
  everything (and makes it old) once the old cells have doubled. Aging
  took binary_trees from 89MB to 63MB, and made it faster. Where the
  young generation cannot grow any more and most of what aging kept young
  survives again anyway (what the program keeps outlives a small young
  generation), aging would only trace it twice: minor collections then
  make cells old the first time, for two collections, before trying
  again. A shape (shared, hot) is checked old by its header rather than
  its chunk's bits.
- **Sweeping.** Dead cells are not visited, but for those with something
  to release (class finalizers, large property arrays and array elements,
  weak references, shapes, function bytecode), whose finalizers run before
  any cell is freed: plain objects, arrays, strings and closures die
  without being touched. An array literal's elements are in the array's
  cell; an array's elements and an object's properties that outgrow its
  cell move to a cell of their own (up to 504 bytes), which the object
  marks, rather than to storage its finalization frees. A minor collection sweeps only the chunks with young
  cells.
- **Weak references.** Strings referenced only as values leave the atom
  table once unreachable, shapes the shape hash; WeakMap entries, WeakRefs
  and FinalizationRegistry callbacks are processed when their targets die.
- **Checking it.** `QJS_GC_STRESS=n` collects every n allocations;
  `QJS_GC_VERIFY=1` poisons dead cells and keeps them for a collection
  (aborting on a reference to one), and after each minor collection checks
  that everything reachable is marked, naming the holder of a reference
  whose barrier is missing. The test suite runs with both;
  `QJS_GC_STATS=1` prints the counts at exit.

### Values: 8 bytes

QuickJS-NG's `JSValue` is 16 bytes on 64-bit platforms: a 64-bit payload
and a 64-bit tag. Patch `0011` makes it 8 bytes: NaN-boxed, as
JavaScriptCore's values are, with a reference to a cell holding its
address / 8 in 32 bits, as V8's compressed pointers do.

- **Encoding.** A double is its own 64 bits. Every other value has a
  high half that no double has, its tag plus `0xfffffff8` (`0xffffffef`
  to `0xffffffff`: negative NaNs that no arithmetic makes), and its
  payload in the low half: an int32, a bool, or a reference. A float is
  then any value below `0xffffffef << 32` (one compare), reading or
  writing a float field is a plain 8-byte load or store (folded into the
  arithmetic), an int or object test compares the high half, and a
  reference's address is `(v & 0xffffffff) << 3`. The engine makes every
  NaN the canonical one; native code does so for the NaNs it reads from
  typed arrays, the only place other NaN bits can come from. (QuickJS's
  own NaN-boxing, for 32-bit platforms, does it the other way round: tags
  from 0 up, a double offset by an add on its way in and out, which made
  `bench/nbody.ts` 60% slower here than with 16-byte values.)
- **The cage.** Every cell (and module record) is below 32 GB: the first
  runtime reserves the largest range of address space it can get there
  (normally 4 to 32 GB, `PROT_NONE`, nothing committed), which every
  runtime of the process shares; the collector's chunks, regions and large
  cells are mapped out of it, and given back to it (pages released) when
  freed.
- **Property slots.** A property is 8 bytes: its value, or an accessor's
  getter and setter as two 32-bit references (the data of the rare
  lazily-initialized property is out of line). Elements, closure
  variables, function data, async frames and the interpreter's stack hold
  8-byte values.
- **Native code.** A value is one register (arguments, results, block
  parameters) and one 8-byte load or store. Async frames, which the
  collector traces, hold only values: numbers (and integer locals) are
  stored there as floats. A tag is no longer a register of its own, so
  the number paths are ordered to need few tests: two int32s, else two
  floats (one compare each), else one of each, converted by branches;
  an operation's result carries whether it is a number, so that the
  next test of it folds away where it came from a number path; a
  region's guard tests a field from its high half alone (a 32-bit
  load), so that the loop reads the field straight into a float
  register.
- **The collector.** A reference is 32 bits, so the conservative scan of
  native stacks and registers looks at each half of each word, as well
  as at the word as an address (anywhere in a cell). A half is taken for
  a reference only where it is a cell's start, and a low half only under
  a reference's tag (or alone): the low halves of ordinary addresses on
  the stack land somewhere in the cage most of the time, and kept what
  they landed in alive (a tree's subtree, in `bench/binary_trees.ts`).

What it saves: peak memory (maximum resident set size) and time of the
benchmarks that keep data, 16-byte against 8-byte values (LLVM backend,
the median of 7 interleaved runs, the run of the table below; bun 1.4.3
for scale):

| benchmark      | 16-byte values   | 8-byte values    | bun              |
|----------------|-----------------:|-----------------:|-----------------:|
| heap           | 184 MB / 312 ms  | 132 MB / 272 ms  | 131 MB / 284 ms  |
| objects        |  80 MB / 470 ms  |  64 MB / 379 ms  |  88 MB / 442 ms  |
| records        |  66 MB / 559 ms  |  54 MB / 463 ms  |  64 MB / 601 ms  |
| arrays         |  41 MB / 128 ms  |  34 MB / 116 ms  |  78 MB / 135 ms  |
| shapes         |  28 MB / 171 ms  |  21 MB / 143 ms  |  45 MB / 114 ms  |
| binary_trees   |  63 MB / 315 ms  |  57 MB / 265 ms  |  55 MB / 412 ms  |
| logparser      | 182 MB / 1076 ms | 175 MB / 1106 ms | 223 MB / 970 ms  |
| tokenizer      | 111 MB / 212 ms  |  87 MB / 190 ms  | 193 MB / 266 ms  |

`bench/heap.ts` (300,000 records of six fields with a Map index, and a
grid of numbers) shrinks by 28%, to bun's size; the others by 4 to 25%,
by what of them is values (strings, which are bytes, stay as they were).
Times move with fewer cache misses and a smaller heap to trace, and
against that with more work per value: a reference is decoded before it
is followed, and a tag compared from the high half. Counted with
callgrind, `bench/heap.ts`, `bench/binary_trees.ts`, `bench/objects.ts`
and `bench/nbody.ts` run 2 to 6% fewer instructions than with 16-byte
values, and `bench/raytracer.ts`, `bench/shapes.ts` and
`bench/records.ts` 9 to 11% more; the engine's C code (most of
`bench/logparser.ts`, in string methods) about 9% more. In time the
differences under 10% are within this machine's drift: nbody, on 2%
fewer instructions, ran from as fast as with 16-byte values to 17%
slower (the decoded reference is on its loads' path), logparser from as
fast to 10% slower.

The collector scans native stacks conservatively: a dead reference that
a register or a stack slot still holds keeps what it references alive
until something writes over it, and how much that keeps depends on how
registers were allocated. One build of this code generator kept
`bench/tokenizer.ts`'s previous round's array of tokens alive through
the next round (147 MB at peak, not 87): a callee-saved register of the
module's code still held it, dead, and the calls into the engine saved
that register on the stack. Keeping the JS values a loop's iterations
declare in slots of their own, cleared at each iteration's end, does
not rule that out (LLVM keeps copies of what it loads from them, in
registers and its own slots), cost up to 8% in hot code, and kept more
alive in `bench/objects.ts`, not less; what rules it out is precise
stack maps for native frames (each value live across a call in a slot
the collector reads, and nothing else of the frame read).

Servers keep little: Beyond's (`Beyond.serve`, Hono, Elysia) peak 1 MB
lower, at 23 to 29 MB.

`-DJS_COMPRESSED_POINTERS=0` builds the engine with 16-byte values (the
generated code needs 8-byte ones), and lifts the 32 GB limit.

## Backends

`--backend cranelift` (the default) or `--backend llvm` (or `LUMINOUS_BACKEND`)
picks the code generator of `luminous build` / `luminous run`. Both translate the
same backend-neutral IR (`mir.rs`), which the lowering builds, with the same
semantics: wrapping integer arithmetic, shift amounts modulo the width,
saturating float-to-integer conversions, NaN-propagating `fmin` / `fmax`.
The differential tests run every case with both. With `LUMINOUS_TIMES`,
a build prints how long its phases took (analysis, lowering, the backend,
clang, linking).

- **Cranelift** compiles in process, in milliseconds, a large program's
  functions in parts on as many threads as the host has (8 at most; one
  object file each, with the functions local to the program hidden,
  rather than local, to the others): the Node.js modules of Beyond's
  tests compile in 8 s instead of 24 s on 4 cores. It optimizes little
  beyond what the lowering does itself (which, for instance, lowers the
  tests of `if` and loops through `&&`, `||`, `!` and `?:` as jumps, so
  `a || b || c` is a compare and a branch each, no boolean made).
- **LLVM** emits LLVM IR (text) and compiles it with `clang -O3
  -march=native` (clang ≥ 15 on `PATH`, or `--clang` / `LUMINOUS_CLANG`;
  `--march` picks another CPU, e.g. `x86-64-v3` for portable executables).
  Compiling takes seconds, and the code is optimized much further. A large
  program's IR (4 MB or more) is split into as many modules as the host
  has threads (8 at most), each defining a run of its functions and
  declaring the others, compiled by that many clangs at once (the
  program's analysis and IR are freed, and their memory returned to the
  system, before they start: a 6 GB compiler and four 2 GB clangs did not
  fit in 16 GB): Beyond's
  Hono app (186 MB of IR) builds in a third of the time (1 min 34 s
  against 4 min 19 s on 4 cores: bundling, analysis and linking stay
  serial), its code running the same instructions per request (67,008
  against 67,159 as one module):
  - the functions that only native code calls are internal to the LLVM
    module, so LLVM inlines them anywhere in the program (recursive `fib` is
    partly inlined into itself), drops the unused ones and picks their
    calling convention (in a split program, within its part: those of the
    others are hidden, and the always-inlined ones copied into each);
  - every load and store carries its alias region as type-based alias
    analysis metadata: cell headers, `JSValue` slots (properties,
    elements, cells, frames), the engine's object headers, typed array
    elements, `aot type` storage, inline caches, constant tables. Values
    then stay in registers across stores to other regions, and dead
    stores go;
  - addresses derived from globals, stack slots and loaded pointers stay
    pointers (`getelementptr`), and the tables filled before any native code
    runs (atoms, strings, shapes) are invariant loads, hoisted out of loops
    and across calls;
  - slow and exception paths become branch weights (laid out of line), and
    the register allocator charges for a function's first callee-saved
    register, so values live across the calls of slow paths are spilled
    there rather than held in registers every call saves and restores;
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
  (folded, unless code that runs before the declaration can reach one: by
  naming it, or naming a function, class or value whose code names it). A
  `switch` on a number whose cases are numeric constants compares numbers.
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
  - string, template, object, array and regular expression literals (a
    template's parts, and a chain of `+` whose first or second operand
    is a string and whose others are strings or primitives, such as
    `a + ":" + b`, are concatenated by one engine call that copies each
    once and writes int32s as digits, rather than one `+` at a time; a
    regular expression's pattern is compiled once, and each evaluation
    makes a new object of it);
  - property and element access, assignment and `delete`. Own data
    properties are read and written through per-site inline caches: a shape
    check (one comparison: other kinds of entries keep their shape in a
    second field), then a load or store. A cache also covers the object's own
    getters and setters, methods, getters and setters found on the first
    or second prototype, and private fields and `super.name`. A
    prototype's method or getter is kept in the cache itself with the
    engine's prototype epoch (see `JS_WatchPrototype`): while no watched
    prototype has changed, a hit is the receiver's shape and the epoch
    compared, and the value (with its native entry, which a predicted
    call compares instead of reading the function) taken from the cache:
    no prototype is read, and no kind dispatched on. After a change to a
    watched prototype, a site whose prototypes' shapes still hold takes
    the new epoch once (a runtime call, out of line), and one that keeps
    seeing changes stops after 64 and checks the shapes on the way, as
    before. A computed member read or write whose key is a symbol (Bun's
    `this[kFd]`, `this[kRefs]++`) has a cache of its own too, of the first
    symbol it sees: the key's reference compared, then the object's shape,
    as a named site's (the runtime's misses fill it as a named one's); a
    site that sees another symbol stops caching. A method site compares the prototype entry's shape
    first, the arguments of a predicted call are spilled only where it
    calls, and the tests of `if` and loops are jumps. A site that sees objects of several shapes
    (a class hierarchy) keeps up to four more own or prototype data
    properties (methods up to the third prototype: a class two superclasses
    up), which its misses check before a lookup; sites that never cache
    stop trying. Those entries also cache what a lookup would find costly:
    an assignment that adds the property (the shape before and after, the
    transition made directly while the prototypes, watched, are unchanged,
    as JSC's put transitions: Hono's eight additions a request, 380
    instructions each, now about 100), and a property that objects of a
    shape and their prototypes lack (`init.statusText` of a plain
    `{ status, headers }`: undefined, without trying every kind of entry
    again). A read site's first such entry takes the site's own fields
    when they are free and is checked inline as a prototype's property is,
    its value `undefined` (a loop reading 2 properties and a missing one:
    156 instructions an iteration, now 123, without a runtime call);
  - `instanceof` through a per-site cache of the right operand: its shape
    and class, and the slot of its `prototype`, valid while its prototypes
    (watched, up to `Function.prototype`) are unchanged; the left
    operand's chain is then walked inline (Hono's 14 a request: 180
    instructions each, now about 50);
  - object literals: each site caches the literal's final shape, and
    later objects are made with it directly (as JSC's allocation profiles
    do), allocated inline: a block popped from the allocator's cache for
    the shape's size class and the object's fields stored in place
    (`JS_ShapeAllocInfo`; the engine call otherwise). `Object.create(p)`
    likewise, while it is the builtin: the site caches the empty shape of
    its prototype (null, or an object), and an argument with that
    prototype gets an object made inline (a loop making them: about 290
    instructions an iteration, now 81; `{}`, 40). An empty one has room
    for 4 properties (patch `0025`);
  - property additions: a write site that added the property caches the
    transition (the shape before, the one after), and native code makes
    it inline: the shape after and the value stored, while no watched
    prototype changed (no setter appeared), the object is an extensible
    ordinary one, and the property fits in the room the shape before
    gives (else the engine, which grows the properties in the object's
    cell while there is room). `{}` and 3 properties added, then 2 read:
    about 600 instructions, now 342; from `Object.create(null)`, about
    870, now 381 (53 and 83 where the object does not escape: see
    "Plain objects that do not escape" below);
  - `typeof`: compared with a string (`typeof v === "string"`), a test
    of the value's tag, and for an object of its class (below the proxy's,
    the function classes are a range; others ask the engine), without
    making the string: 7 ns, now under 1 ns (bun 0.9 ns). Alone, the
    name's string constant, from the same tests (7 ns, now 4.5);
  - `in`, `new`;
  - remainders: of two int32s (`i % 4`), an integer remainder, but for a
    zero divisor or a zero from a negative dividend (-0), a double; `x %
    1` of a double (`n % 1 === 0`, whether `n` is an integer), `x` less
    its integer part, with its sign, where both called `fmod`;
  - `arguments.length` in a function that reads its `arguments` only so
    (not written, deleted, or read by its arrow functions): the count,
    which its JS entry passes instead of making the arguments object
    (Bun's `readSync`: `if (arguments.length <= 3 || ...)`);
  - BigInt literals below 2^64 (`JS_NewBigUint64`); a function with a
    larger one stays interpreted (Bun's `readSync` was, for its
    `9223372036854775807n`, and Buffer's `checkInt`);
  - calls to any function: native functions directly (small ones without
    loops or closures are inlined, as JIT compilers inline them; a small
    one that calls itself has its own body inlined once, so that each call
    does two levels of the recursion: `fib`, tree walks); a callee only known
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
  - assignment operators, logical ones included (`??=`, `||=`, `&&=`:
    the target is evaluated once, and the right-hand side only when it
    is assigned); an anonymous function assigned to a name takes it;
  - globals.

  Unboxed values are boxed where they meet JS values.
- **Arrays and typed arrays**, with inline element access and `.length`.
  Typed array elements are unboxed numbers: `s += a[i] * b[i]` over two
  `Float64Array`s is plain floating-point code. An out-of-bounds read is
  `undefined`, encoded as a reserved NaN inside native code.
- **Source text.** `toString` of a native function gives its source, as the
  interpreted program's does (frameworks read handlers' parameters from it):
  a function bound to its interpreted fallback (a declaration, a method)
  gives the fallback's; a closure, its text without types, kept in the
  glue (printed on its own: its indentation may differ).
- **Closures**: nested functions and arrows, including ones that outlive
  their creator and capture loop variables (one binding per iteration, as in
  JS). A captured variable never assigned after its declaration is
  captured by value: no box is allocated for it. So is a parameter never
  assigned (in a function without `arguments`), and a variable of an
  async function or generator, whose suspended frame keeps the value
  (Hono's compose and dispatch: 13 boxes a request, now 2; 5,300
  instructions). A box is one cell: the value in the box object's own,
  which needs no finalization (it was an object holding a separate
  traced block). Closures of one function
  share a shape, and after the first one native code makes them inline as
  it makes object literals: one cell bumped off its size class's run,
  holding the function object, its `length` and `name`, its record and
  its captured values, stored from a template the first one left
  (`JS_CFunctionDataTemplate`; 10M closures in 145 ms, 350 ms through the
  engine). Async arrows and methods are made of AsyncFunction.prototype
  from the start, in one shared shape (they were given it one by one,
  each then with a shape of its own), and inline too. Side effects on module-level bindings (`count++`) are shared with
  interpreted code. An arrow function's `this` is the `this` of the method,
  constructor (after `super(...)` in a derived class) or object literal
  method it is in, captured in a box as a variable is.
- **`arguments`** (unmapped, as in strict code: assigning a parameter
  does not change it), also in the arrow functions inside the function,
  which capture it in a box as they capture `this`. The function's JS
  entry makes it from the arguments it was called with (its last
  parameter); native code calls such a function through its JS entry.
- **Recursion without return types**: a function without a return type
  annotation that calls itself (or calls a function that calls it back)
  before its return type is known returns any JS value, boxed, whatever
  it returns (with an annotation, its own type, unboxed).
- **Ordinary functions** (`function` declarations and expressions, not
  methods or arrows) are constructors, as in JS: a native one has a
  `prototype` (made when first read), and `new f()` makes an object of
  it, runs `f` and returns the object unless `f` returns one (so
  `function Foo() {}` with `Foo.prototype.m = ...`, older JavaScript's
  classes, works). One that uses its `this` gets it as methods do: the
  receiver of `o.f()`, the object `new` makes, what `call`, `apply` and
  `bind` give, and undefined in a plain call (all compiled code is module
  code, strict), which native callers pass (a function assigned as a
  method that replaces itself, as Hono's router does: 1,000 instructions
  a request it ran interpreted).
- **Class methods, getters and setters** (`this` included; static ones
  too), with the class's
  private fields, methods and accessors (`this.#x`, `this.#m()`,
  `#x in obj`) and `super.x` / `super.m()` / `super.x = v`. Private
  instance methods are compiled too, and native `obj.#m(...)` calls them
  directly after the brand check (a private method cannot be replaced);
  interpreted code calls the class's own.
- **Classes written as class expressions** bound at the top level of a
  module (`var X = class {...}`, or `let` / `const`, as bundlers and
  compilers write class declarations): compiled as the declaration
  `class X {...}` is (methods, constructor, native `new X()`), when nothing
  can tell the binding from that class's: it is never reassigned, and no
  code that runs before it can reach it (to see it `undefined`, or in its
  TDZ). A name of its own (`var X = class X {...}`) is the binding. Other
  class expressions stay interpreted.
- **Classes inside functions** (declarations and class expressions, in
  functions, closures and module code): made natively each time their
  definition runs, as the engine makes a class. The constructor is a
  native closure over the function's variables that `new` calls from
  anywhere (interpreted code, `Reflect.construct`, a subclass's
  `super(...)`), with `this` new.target: a base class's makes the
  instance of new.target's `prototype` and defines its fields in order; a
  derived class's `super(...)` constructs the class's prototype as it is
  then (the class is the function running), then defines its fields.
  Calling it without `new` throws. Its superclass is checked as the class
  definition checks it (a constructor or `null`, its `prototype` an
  object or `null`). Methods, getters, setters and static methods (async
  and generator ones too, computed keys evaluated in order and naming
  their functions) are native closures defined on the prototype or the
  class, not enumerable; TS parameter properties are assigned as for
  top-level classes. Each evaluation is a class of its own (a class in a
  loop sees that iteration's bindings). `toString` gives the class's and
  the methods' source text. Elysia's class of each request's context
  (`class Context extends Base` in a function) is one: each request
  constructs it natively.
- **Class constructors** of classes whose instances are plain objects
  with their fields (no accessors or computed keys, initializers that do
  not use `this` but in arrow functions that are a field's value, a
  constructor that returns nothing but the instance), TS parameter
  properties (`constructor(public x: number)`) included. Native `new
  C(args)` calls the native constructor directly (after reading `C`,
  which may be in its TDZ): the instance is made with the class's fields,
  initializers evaluated first, through an allocation site that caches
  the instance's final shape, then the body runs. Private fields, and the
  brand that private methods and accessors check, are part of that shape
  (after the public fields: nothing can enumerate them), so a later
  instance is made with all of them at once. Fields that are arrow
  functions seeing the instance (`json = (o) => this.#send(o)`, as
  frameworks write handlers) are made right after it, with `this` boxed:
  making a function runs no code. A constructor that only assigns its
  fields (`this.x = x`, values that run no code) makes the instance with
  them directly, so an inlined `new V(a, b)` is an object expression.
  Interpreted code keeps the class's own constructor.
- **Constructors of derived classes** (`class B extends A`) that start
  with their only `super(...)` call (arguments and parameter defaults that
  cannot see `this`), or have none (the default one passes its arguments
  on). Its fields are defined in order after it, and may use `this`. When
  `A` has no native constructor (`Error` and the other built-ins, an
  interpreted class, any other expression), `super(...)` constructs the
  superclass as interpreted code does: the class's prototype at that
  moment, with `new.target` the class constructed (`class HttpError
  extends Error`, and its subclasses, are native; their instances are
  errors, with their stack). When `A` has one, each
  superclass constructor is also compiled once per subclass to make that
  subclass's instances (`new.target` known), so `super(...)` is a direct
  call that returns the instance; the subclass's fields are then defined
  (never through a setter) and its parameter properties assigned. After
  inlining, a whole chain of such constructors makes the instance with
  all its fields in one allocation. `new B()` first checks that each
  class of the chain still has its declared superclass as its prototype
  (`Object.setPrototypeOf(B, X)`): if not, the class is constructed as
  interpreted code would. Parameter defaults that may run code (they are
  not literals, or object and array literals of them: `options = {}`)
  run after that check, so `super(...)` checks the chain again, and if
  it changed constructs the superclass as interpreted code would.
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
- **Plain objects that do not escape**: a local holding an object
  literal or `Object.create(null)` whose properties are added after it
  is made (`const o: any = {}; o.a = i; o.b = 2;`), used by the
  statements after its declaration in the same block, keeps its
  properties in locals while a flag says it is virtual: an addition is a
  statement `o.k = v` of that block, and the statements after it read and
  write `k` there; a key `Object.create(null)` was not given reads
  `undefined`, and a comparison with `null` or `undefined` is known. An
  addition goes through the prototype in JS, so for an object of
  `Object.prototype` it is checked (`PlainAddOk`, kept with the prototype
  epoch: one comparison) that the prototype has no setter or read-only
  property of the key; `Object.create(null)` is virtual while it is the
  builtin, else its result is used. Anywhere else (the object passed or
  returned, a key it lacks, an addition in a branch, `delete`, a setter
  that appeared) it is made from the properties it has, once, and that
  use and the later ones work on it: a setter then sees the object as it
  is. A new object, 3 keys added and 2 read: 342 instructions an
  iteration, now 53 (`Object.create(null)`: 381, now 83); a new `{}`
  compared with `null`: 40, now 8; a function that adds 3 keys to `{}`
  and returns it: 322, now 116 (the object made once with its final
  shape).

  These passes (and those of `aot type` objects) run after inlining
  (`ir::replace_objects`), on what the inlined calls left: an inlined
  call reads its arguments' locals itself (a parameter bound to a local
  the call does not assign is that local), the body of one whose value a
  statement takes (`const q = make(i)`, `return f(x)`, `await g()`) runs
  as the caller's own statements, and that of an expression and a
  `return` is a sequence. So an object passed to a small function is read
  there through its fields, and one a small function makes and returns is
  the caller's: `len({ x: i, y: 2 * i })` with `len(p) { return p.x +
  p.y }` is two locals added, without an object or a boxed number. Along
  the code after the declaration, the virtual flag is folded where it is
  known (`escape::fold_flag`): true until something may have made the
  object, false after something surely did; an object always passed on
  is then made where it is passed, and read there after, without tests.
  (An `async` callee is not inlined: to keep the order of promise
  reactions, the caller would have to settle a promise of its own
  wherever the callee's would settle, and adopt a returned promise in
  as many ticks.)
- **Predictions from type annotations**: where an annotation names a
  top-level class (`o: Vec3`, `xs: Sphere[]`, a field `center: Vec3`, a
  method or function returning `Vec3`, `Node | null`), calls on those
  values are predicted to reach that class's native methods, as for
  `this`: a small method is inlined behind a check that the method reached
  is that one. Annotations are never trusted otherwise: a wrong one only
  costs the check. What an annotation says follows the values: the
  elements of an annotated array (`xs[i]`, `for (const x of xs)`, the
  parameters of arrow functions passed to `xs.map`, `filter`, `forEach`,
  `sort`, `reduce`, ...), and what `slice`, `filter`, `sort` or `find`
  return.
- **Polymorphic call sites**: where such a receiver (or `this`) may be an
  instance of the class or of its subclasses, which have methods of their
  own (`s: Shape` with `Circle` and `Rect` overriding `area`), each of the
  methods is predicted in turn (up to four): the method the receiver
  reaches is read through the site's inline cache, whose other shapes'
  entries are checked inline there, and compared with each; the one it is
  runs inline, any other is called. A derived class's records list its
  superclasses' fields then its own, so the inlined methods read fields of
  subclass instances at fixed slots too.
- **Fields of annotated object types**: where an annotation names an
  object type alias, an interface or a class with plain public fields
  (`b: Body`, `bs: Body[]`, a field `pos: Body`), its fields are read and
  written at fixed slots. The first objects of the type seen with the
  declared fields first, in order, as writable data properties give the
  type's shapes (up to four: a class's own instances and its
  subclasses'); each access then compares the object's shape with the
  first one (one load and one comparison, shared by all the type's sites),
  then the others, and loads or stores the slot. Any other object (fields
  in another order, missing ones, accessors, frozen) goes through the
  site's inline cache.
- **Speculative numeric regions**, as JIT compilers speculate on what
  fields hold: a run of statements that only does arithmetic on fields
  annotated `number` (of locals holding such objects), numbers and untyped
  locals (`const dx = a.x - b.x; ... a.vx -= dx * b.m * f;`) gets a second
  version where all of these are doubles. One guard before it checks the
  objects' shapes, that they are distinct objects if fields are written,
  and that every field and untyped local it reads holds a number (each tag
  compared with the float tag in turn, then one OR of the tags for
  int32s); the run then reads the fields as doubles where it uses them,
  computes in registers, and stores the fields it changed at its end (as
  floats, as the interpreter's float arithmetic makes them, leaving their
  float tags in place; a copy for when some held int32s stores int32s
  where the results are). Nothing in the run can call or run code, so
  what the guard checked holds throughout. When it fails (a string, a BigInt, the
  same object twice, a getter), the statements run as before. Runs may
  contain `if`, `?:`, `&&`, `||`, comparisons, `return`, loops (with
  `break` and `continue`), calls of leaf functions on numbers, and
  `s.charCodeAt(i)` / `s.length` of strings in locals: the guard then also
  checks that those are flat strings and that `charCodeAt` is the builtin,
  so a scanning loop such as `while (i < n && isDigit(s.charCodeAt(i)))
  i++` runs as integer code. A string's length is an int32 (a loop
  counter below it needs no overflow check), and a local only ever
  assigned characters' codes (and integer constants) keeps the code as an
  integer too, -1 standing for NaN past the end: `c === 0x22`, `c >= 0x7f`
  are integer compares, and need no in-bounds flag where -1 compares as
  NaN does (`===`, `!==`, `>`, `>=` with constants from 0). A region may
  return a boolean. Methods take part: `this` is guarded as
  locals are, objects held by guarded objects' fields (`this.center`)
  are guarded too, a predicted call of a class's method runs its body in
  the region (the guard checks that the receiver's prototype is the
  class's and still has the method, and that the class binding is
  initialized), and the vectors such methods return (scalar-replaced, see
  below) keep their fields in float locals, so `origin.sub(this.center)
  .dot(dir)` is float code throughout (a method body with an early
  `return` runs inline too). A run that stores no field needs enough
  arithmetic (or a loop) to be worth its guard. Untyped locals that hold
  int32s are converted once and do not keep the fields' float code from
  running.
  A loop over the objects of an array (`for (let j = i + 1; j < n; j++) {
  const bj = bodies[j]; ... }`, `spheres[i].center.dot(dir)`) can be a
  region as a whole: the guard checks once what the iterations share (the
  array is a dense fast array, so `arr.length` is its element count and
  stays so; the other objects; the methods), and each iteration starts by
  checking its element: its shape, that the fields the loop uses hold
  numbers, the objects its fields hold, and that it is none of the
  objects whose fields the region keeps in registers. Elements' fields are
  read and written in place: an iteration has two copies, one where they
  all hold floats (read as doubles, their tags left in place) and one
  where some hold int32s. Where an element is not what
  the region needs (another shape, a string field, a hole, the same object
  as another), the region stores what it changed and the original loop
  goes on from that iteration (what each iteration declares afresh, its
  own locals and the fields of the vectors it makes, needs nothing from
  the guard). With methods passing their parameters on to other inlined
  methods (`oc.dot(dir)`), the ray tracer's whole sphere loop
  (`spheres[i].hit(origin, dir)`) is one region.
  A loop in such a loop's body may read elements too (`const bi =
  bodies[i]; for (let j = i + 1; j < n; j++) { const bj = bodies[j]; ...
  }`): the whole nest is one region, the outer element checked once per
  outer iteration, the inner one in each inner iteration (and checked not
  to be the outer one where the iteration writes elements' fields). Where
  an inner element is not what the region needs, the original code goes
  on from that inner iteration: the outer iteration's elements named
  again, the rest of the inner loop, the rest of the outer iteration, the
  outer loop from its next iteration. An n-body step is one region.
  A jump out of a region from inside its loops (`break` or `continue` of an
  enclosing loop, `return`) stores every field the region keeps in a
  local and every local it may have assigned, in this iteration or in an
  earlier one (those the guard loads first).
  A loop's elements may be numbers too: `s += xs[i]` over a `number[]`
  checks in each iteration that the element is an int32 or a float (else
  the original loop goes on from it), and reads it as a double. Typed
  arrays need no such check: with a pristine shape, a typed array's
  `length` is its element count, the iteration checks that its index is
  inside the arrays it reads, and reads them in place (so a write earlier
  in the iteration, through any view of the buffer, is seen); writes check
  their index and do nothing outside the array, as the engine does, and
  the guard checks that the buffer is not immutable. A sieve's inner
  loop, `dot` over two `Float64Array`s and `sum` over a `number[]` run as
  plain loops over doubles.
  A region may append numbers, or object literals of numbers, to arrays
  in locals (`tokens.push({ kind: 1, start, end: i })`): the guard checks
  that each is an extensible fast array of a fresh array's shape whose
  `length` is its element count, that `Array.prototype.push` is the
  builtin and that the array prototypes have no index properties (as the
  engine's fast path); the region then stores each value directly (the
  array growing through the engine when it is full), so a tokenizer's
  whole scanning loop is one region. Such a region keeps no field's value
  in a local and is not inside `try`, so an allocation that throws (out
  of memory) leaves nothing undone that anything could see.
- **Builtins**: method calls on strings read `String.prototype`'s methods
  through their inline caches; `s.charCodeAt(i)` runs inline,
  `arr.push(x)` appends inline to an extensible fast array of a fresh
  array's shape with room for it (else takes the engine's fast path),
  and `s.slice`, `substring`,
  `indexOf`, `includes`, `startsWith` and `endsWith` on a string call the
  engine's implementation directly (no call frame; int32 bounds taken
  as they are) while they are the original builtins (checked at each
  call); `Object.keys(o).length` counts the keys without making the array
  while `Object.keys` is the builtin (`JS_CountOwnKeys`, patch `0022`; an
  object of an `aot type`, its fields present): 42–46 ns, now 13–14 ns
  (an `aot type` object, 311–342 ns, now 14–17 ns; bun 12–20 ns);
  `===`, `!==` and `switch` against
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
  callback may change the array), and calls the builtin otherwise. The
  results of `map` and `filter` are appended to inline: when the Array
  constructor made the result itself (no species constructor ran), no
  other code holds it, so an element is stored wherever there is room,
  with no other check.
  `xs.sort((a, b) => ...)` with an arrow that captures nothing is sorted
  by a merge sort compiled per comparator (the engine's stable algorithm:
  insertion sorts of up to 8 elements, halves merged), which runs a copy
  of the comparator inline at each comparison; the engine takes the
  elements out of the array and puts them back.
- **Leaf functions** that can neither throw nor call (only unboxed numbers
  and booleans) skip the stack check, and their callers the exception
  check after the call.
- **`async` functions and arrows**: `await` anywhere in an expression,
  including `finally` blocks and loop updates. Awaits are hoisted into
  statements while keeping JS evaluation order, so microtask ordering
  matches the interpreter exactly. `await Promise.resolve(x)` awaits `x`
  itself while `Promise.resolve` is the intrinsic one (the same ticks).
  A call's frame (its locals across awaits) is one cell, the frame object
  with its slots after it, set once rather than zeroed and set again, with
  nothing to finalize; an await resumes it from its own job, queued where
  the awaited promise's reaction job would be. An await continues at once,
  without suspending, when that could not be told apart (as Bun's AOT
  compiler does): the frame runs last in its resumption job, no other job
  is pending and no promise hook watches (so its resumption would be
  queued alone and run next), and the value is a primitive or a fulfilled
  promise of `Promise` without properties of its own (whose reaction would
  be queued at once). `await` of a resolved promise in a loop: 338
  instructions, now 96 (Bun's JIT 458); an await that suspends costs about
  10 more than before. An async function that
  never awaits has no frame: it runs as other functions do, and its entry
  makes the promise of its result. `bench/micro/async.ts`, ns per
  iteration on a 4-core x86-64 VM (before these changes; bun 1.4.3):
  `await` of a primitive 49 (140; 48), of an async call 57 (142; 37),
  nested async calls 136 (436; 154), `await new Promise(...)` 155 (210;
  149), a promise resolved later 190 (366; 222), `Promise.all` of four
  async calls 480 (3,293; 479), a chain of two `then`s 196 (900; 166), a
  three-deep middleware chain each awaiting the next 874 (2,461; 1,580).
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
  keys, nothing enumerable on the prototype chain): the shape's keys,
  shared by every loop over objects of it (`JS_ForInStart`), are read by
  index in the loop itself, with no call or allocation while the object
  keeps its shape (if it changes, each key is looked up again, as the
  engine does); `obj[k]` with the loop key reads the slot directly while
  the shape is unchanged.
- **Destructuring** in declarations, parameters, `catch`, loop heads and
  assignments; **spread** in calls, `new`, and array and object literals;
  **rest** parameters and elements; **optional chaining**; computed keys in
  object literals (of values, methods, getters and setters, named after
  their key); `__proto__: value` in object literals (the prototype, for an
  object or null).
- TypeScript **`this` parameters** (`function f(this: T, ...)`): only a
  type.
- **Object literal methods, getters and setters** (`this` included;
  generator and async methods too; `super` in them
  is not supported yet).

Not yet: constructors of derived classes that run code before
`super(...)`, and constructors using `arguments` (which stay
interpreted), private accessors' and static private methods' direct calls, private names,
static fields and blocks, computed field names and `super.x` in classes
inside functions, `new.target`. A
function using one of these stays interpreted (and functions that call it
call it through its binding). `luminous report file.ts` says, for every
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
- `f.toString()` of a native closure gives its source text printed anew
  (without types; its whitespace may differ).
- Native frames do not appear in `Error.stack`.
- `Object.getOwnPropertyNames(Float64Array.prototype)` includes `length` (see
  above).
- A class whose native methods use its private names or `super` gets a
  static block first in its body (handing them to native code), which shows
  in the class's `toString()`.

## Modules

A program can be several modules: `luminous build main.ts` follows the
`import`s of `main.ts` (and theirs) and merges the modules into one
(scope hoisting, as bundlers do), which the compiler then sees whole:
calls between modules are direct calls that can be inlined, and a class
imported from another module has its native constructor and fixed field
slots. Splitting `bench/raytracer.ts` into three modules runs the same
instructions.

- **Resolution.** Relative and absolute specifiers name files as
  TypeScript resolves them: as written, a `.js` specifier's `.ts` file,
  with `.ts`, `.mts`, `.js` or `.mjs` added, or a directory's `index`.
  Other specifiers are packages, from the nearest `node_modules` up: a
  package's `exports` (subpaths and `*` patterns; the `import`, `module`
  and `default` conditions, the first the package lists that applies),
  else its `module` or `main` field, else its `index`; `#name` specifiers
  are the importing package's `imports`. Their JavaScript is compiled like TypeScript without types
  (types come from the package's `.d.ts` files, which the build does not
  read: an imported name or `ns.Name` that only they declare is a type,
  erased). The engine's modules (`qjs:os`, `qjs:std`) stay imports.
- **Packages.** `node_modules` as `bun install` (or npm, pnpm) lays it
  out works: ESM packages such as `zod`, `valibot`, `date-fns` and
  `lodash-es` build and print what Bun prints. A module whose package says
  it has no side effects (`"sideEffects": false`, or a list of the files
  that have) is part of the program only if the program uses something it
  declares (a binding, or a namespace object that holds one), as bundlers
  do: a package's `index` that re-exports all of its modules brings only
  those used (`date-fns`: 69 native functions rather than 714). CommonJS
  packages (`module.exports`, `require`) are an error for now, and
  packages that need Node's APIs (`node:fs`, `Buffer`, `process`) need a
  runtime that provides them (Beyond). A large program builds much faster
  with the Cranelift backend than with LLVM (`zod`: 18 s against 141 s).
- **Semantics.** Each module's top-level names stay its own: a name two
  modules declare, or that a global another module uses has, is renamed
  in one of them (where no scope that refers to it declares the new name),
  so imports are the exporting module's own bindings, live. Modules run in
  ESM's order (dependencies first, in the order of the `import`s; in a
  cycle, the module that started evaluating is not waited for, and its
  `let` bindings throw until it runs). Default exports (an anonymous
  function or class named `default`), namespace imports, `export *`,
  `export * as ns` and `export { a as b } from` resolve as in ESM, and a
  re-export that does not resolve (missing, ambiguous through `export *`,
  circular) is an error, as are duplicate export names and the module's
  other early errors. Assigning to an import binding throws a TypeError.
  `ns.x` is the binding itself where it is read; `ns` as a value is a
  module namespace object (a Proxy with its semantics: live, throwing
  while a binding is uninitialized, not writable, no prototype). An
  import whose bindings are used only as types is elided, as TypeScript
  does: a module imported only so does not run (an import of a value it
  does not use still runs it). `with { type: "json" }` (or a `.json`
  file), `"text"` and `"bytes"` import a file's content as `default`.
- **`import()` and `import.meta`.** A module that `import()` imports
  with a string specifier is part of the program, compiled with the
  others. One that only `import()` imports (with its own dependencies) is
  evaluated lazily: when an `import()` of it runs, in a later job, once (a
  module that throws rejects every import of it with its error; one that
  awaits at the top level is awaited). Its top-level declarations that run
  no code (functions, classes that run no code when defined, variables of
  literals or functions) stay top-level declarations of the program,
  compiled as those of the other modules; its other top-level statements
  become functions that its evaluation calls. `import()` of a module that
  runs anyway returns its namespace object. A specifier that is computed,
  or that does not resolve when the program is built, is left to run time:
  the build warns, and the engine loads the module (relative to the
  importing module's file) and interprets it, which `console.warn` reports
  the first time; an embedder may load it instead (a function at
  `globalThis[Symbol.for("luminous.import")]`, given the specifier, the
  importing module's directory and the options). Such a program
  registers its modules (`globalThis[Symbol.for("luminous.modules")]`, a
  map from each module's path to a function that returns its namespace),
  so that a module loaded at run time that imports one of them gets the
  program's instance, not a second one. `import.meta` has `url`,
  `filename`, `dirname`, `dir`, `file`, `path`, `main` and `resolve()`
  (the embedder's resolution at
  `globalThis[Symbol.for("luminous.resolve")]`, else a URL relative to the
  module's). `import.meta.require("x")` (Bun's synchronous import, with a
  string) is an `import()` that returns the namespace object itself: a
  module only it imports is evaluated when the first call runs (its errors
  thrown by every call). The modules the embedder's loader is given
  (`luminous.modules`) include lazy ones, evaluated when first given.
  Before an eager module runs, the eager modules its synchronously
  required lazy ones import (directly or through other lazy ones) run
  first, unless they import it themselves, so a lazy module required while
  the program starts finds its imports initialized.
- **Conformance.** test262's module tests (`language/module-code`,
  `language/import`, `language/export`): 622 of 722 pass, against 611 with
  the engine's own module loader (QuickJS-NG); JSON and text imports pass
  here only, and an async module that runs while a sibling waits there
  only. Most of the rest are proposals the engine lacks (`import defer`,
  immutable `bytes` buffers, source phase imports). `import()` and
  `import.meta` (`expressions/dynamic-import`, `expressions/import.meta`,
  all run as modules): 776 of 964, against 757; one test of an async
  cycle whose root throws fails here only (its other members do not
  record the root's error).
- **Not yet:** CommonJS (`require`, `module.exports`), `export =`. In a
  lazily evaluated module, a variable that a statement declares is
  `undefined`, not uninitialized, before the module runs, and a class that
  runs code when it is defined (or extends a class of a module that is not
  lazily evaluated) is a class expression, which stays interpreted. Errors
  point into the module they are in; stack traces of interpreted code show
  the merged program's lines.

## Embedding: runtimes built on Luminous

A runtime (Beyond) builds programs with its own modules, native code and
event loop. `luminous build` (and the `luminous_cli` library: `build` with
`BuildOpts::embed`) takes:

- `--prelude FILE`: a module that runs before the program's entry, as if the
  entry imported it first (where a runtime installs its globals);
- `--module-dir PREFIX=DIR`: specifiers that start with `PREFIX` name the
  modules of `DIR` (`--module-dir beyond:=lib`: `beyond:http` is
  `lib/http.ts`), compiled with the program;
- `--c-module SPECIFIER=FUNCTION`: a native module, made by
  `JSModuleDef *FUNCTION(JSContext *, const char *)` (with the engine's
  `JS_NewCModule`); its imports stay imports;
- `--init-fn FUNCTION`: `int FUNCTION(JSContext *, int argc, char **argv)`,
  called before the program runs;
- `--loop-fn FUNCTION`: `int FUNCTION(JSContext *)`, the event loop that runs
  after the program's module code, instead of the engine's. The module is
  evaluated without waiting for it: a top-level `await` waits on that loop
  (an error the module throws at once ends the program; one after an
  `await` is printed when the loop ends, unless the embedder reported it);
- `--global NAME=SPECIFIER`: the global `NAME` (which a prelude defines) is
  the binding `NAME` of the module `SPECIFIER`: the program's own modules
  that refer to it without declaring or assigning it get an import of it
  (at the end of their text), so that native code knows it (`new
  Response()` calls a class's native constructor);
- `--link ARG`: an object file, static library or linker argument;
- `--alias SPECIFIER=TARGET`: the specifier `SPECIFIER` (exactly) is
  `TARGET` (`--alias fs=node:fs`), resolved before anything else;
- `--shared DIR`: the modules of `DIR` are registered (see `import()`
  above) even when no import is left to run time, so that what the
  embedder loads at run time shares them.

A `declare const x` (any ambient declaration) names the global `x`: its
references stay global reads, whatever other module has a binding `x`.

`tests/embed` is an example, built by `crates/luminous-cli/tests/embed.rs`.

The bundler itself is a library (`luminous_front::bundle`): `bundle_host`
takes a `Host` that resolves each specifier (to a file or an external
module) and loads each module's text (script, JSON, text or bytes), so a
runtime can put a bundler's plugins in between (Beyond's `Beyond.build`,
Bun's `Bun.build`). With `Embedding::library`, the bundle is an ES module
for other programs rather than a program: the entry's exports stay
exports, an `import()` that does not resolve is an error, and it is tree
shaken as Bun and esbuild shake one: a module runs only if a module that
runs imports it or something it declares is used (or, with
`"sideEffects": false`, only the latter), and of each module only its
statements that run code and the declarations that what is kept uses
are kept.

## The `int` type

`type int = number` declares an integer type that the compiler checks:
an `int` variable, parameter or return value always holds an int32 (never
`-0`). TypeScript sees a plain `number`, so the program runs unchanged with
`tsc`, Bun or Node; Luminous keeps `int`s in machine registers.

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

aot type C = { nested: { notNested: string } };
aot type D = C[];
aot type E = { on: boolean; items: D; more: { id: int }[]; tree: Tree };
aot type Tree = { label: string; children: Tree[] };
aot type User = { name: string; age?: int; email: string | null; boss: User | null };
aot type Maybe = { x: number } | null | undefined;

const p: Point = { x: 0, y: 0, z: 0, kind: "colored", color: "#fff" };
const e: E = { on: true, items: [{ nested: { notNested: "a" } }], more: [], tree: { label: "t", children: [] } };
const u: User = JSON.parse(text);              // parsed to the type: only its fields
const v: User = await (await fetch(url)).json();
```

- **Members and fields.** An `aot type` is object types combined with `|`
  and `&` (and other `aot type`s), at the top level of the module. Fields
  are `int`, `number` / `double`, `boolean`, `string`, string literals,
  another `aot type` (its own included: `children: Tree[]`), object types
  written in place (`nested: { notNested: string }`: an `aot type` of its
  own, `C.nested` in errors) or an array of objects of one of these
  (`T[]`, `Array<T>`, `{ id: int }[]`). An `aot type` can also name such an
  array (`aot type D = C[]`), for fields, bindings and parameters; so can
  a plain alias (`type Y = Array<A>` is `A[]`). A field may be optional
  (`age?: int`, or `int | undefined`) and may hold `null` (`string | null`,
  `User | null`); an `aot type` may also hold `null` or `undefined`
  (`Maybe` above: a parameter of it takes either, checked on entry). No
  methods, index signatures or type parameters, and no arrays of
  something that may be `null`.
- **One layout.** All the members' fields share one layout, with a mask of
  the fields an object has. An object is valid when it has every field of
  some member, with that member's types, and its other fields belong to
  some member (so `{ a } | { b }` holds `{ a }`, `{ b }` and `{ a, b }`).
  Fields that every member stores the same way are unboxed: an `int` is an
  int32, a `number` a double, a `boolean` a byte. An optional field is
  absent from an object until set (its bit in the mask clear); reading it
  gives `undefined` then, and native code reads an optional number as a
  number-or-`undefined` double, unboxed. Setting it to `undefined` or
  deleting it makes it absent again. A field that may hold `null` holds a
  JS value.
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
  interpreted code alike (optional fields are added and deleted: they are
  configurable). An `int` field wraps other integers to int32 (as
  `| 0`; with `--no-int-wrapping`, a TypeError); a write that would leave
  the object matching no member is a TypeError. `Object.freeze` throws.
  An array of them can be frozen or sealed (`Object.freeze`,
  `Object.seal`, `Object.preventExtensions`, or `defineProperty` of its
  elements and `length` once it is not extensible), as a whole: its
  elements, or its length, are then read-only for native and interpreted
  code alike, and reads stay as fast.
- **Arrays of them.** An array literal or `new Array(n)` (or `Array(n)`)
  where `T[]` / `Array<T>` is expected makes an array that only holds
  objects of `T` (or holes): storing anything else in it is a TypeError,
  while to JS it is still an array (`Array.isArray`, JSON, spread and the
  `Array.prototype` methods work; `push`, `unshift`, `splice` and `fill`
  arguments that are literals make objects of `T`). Native code reads an
  element as an object of `T` or `undefined` (reading a field of
  `undefined` throws as in JS), and so field reads through it are typed.
  A `let` / `const` declared `T[]` that is only ever assigned such arrays
  is typed everywhere, module bindings included. A field of an array type
  holds such an array: an array literal in its place makes one, and
  storing any other array there (a plain one included, even of objects of
  `T`) is a TypeError; `xs[i].f`, `xs.length` and `xs.push(...)` through
  it are typed as for a binding.
- **Native code** reads and writes fields at fixed offsets: on a value of a
  declared `aot type` (a literal, a parameter, a field, an element of an
  array of them: `xs[i].x = v` checks the element is an object after
  evaluating `v`, as JS does), `p.x` is one load of an int32 and arithmetic
  on it stays in integers. Elsewhere (an element
  of a plain array), property caches recognize the object's layout. An
  object that does not escape is not allocated at all: its fields become
  locals, with the same checks on stores. In a loop that runs no other code
  (no calls, getters or conversions), the fields it uses live in registers
  and are stored back after the loop. A native function whose `aot
  type` parameter gets another value declines the call (the interpreted
  version runs). A call whose argument of an `aot type` is not known
  to be one (an element of an untyped array) checks it at the call and
  calls the native function directly.
- **JSON.** `JSON.stringify` (without a replacer or indentation) writes an
  object of an `aot type` from its fields, in their order, and an array of
  them element by element, holes as `null` (`JS_SetJSONWriter`, patch
  0005). Each layout has a writer generated with the program (as Bun's AOT
  compiler makes one per type): straight-line C, keys as constants, values
  read at their offsets, numbers, booleans and strings of up to 16
  characters written in room reserved once (`0019`); a layout whose keys
  need escapes is written field by field. Native code calls
  `JSON.stringify` directly while it is the builtin, and where the value
  is statically of an `aot type`, its layout's writer: `{ id, name, x, y }`
  1,466 instructions, now 577, as Bun's AOT compiler with the type (578).
  The output is the general algorithm's; a `toJSON` on `Object.prototype`
  or `Array.prototype`, another prototype for an array, or nesting deeper
  than 32 sends the whole call to the general algorithm.
  `bench/micro/json.ts`, a 7-field object with a nested one, on a 4-core
  x86-64 VM: about 200 ns, as bun 1.4.3 (190–230 ns; 2.2 µs before the
  writers; the same plain object, 230–320 ns), and an array of 100 of them
  12–15 µs, about half of bun's (22–29 µs; 170 µs before; plain, 23–29 µs).

### From JSON

`JSON.parse(text)` and `await r.json()` where an `aot type` (or an array of
them) is expected (a binding, a returned value, an argument, a field, an
element, or `as T`) give one: the value converted, each field of the type
read from the object's own properties (others ignored), checked as a store
would check it, and nested objects and arrays converted the same way; a
value that does not fit is a TypeError (the first field, in the type's
order, that does not), as is one missing a field.

Native code parses the text to the type directly while `JSON.parse` is
the builtin (`lum_json_parse_as`, from a string's Latin-1 characters in
place: `JS_GetStringLatin1`, patch `0021`): only the fields of the type are
made, other keys and values skipped (checked, not made), numbers stored
unboxed (exactly when they have at most 15 significant digits and a power
of ten up to 22, Clinger's fast path; else `strtod`). Keys are matched
against the field after the last one first (the order `JSON.stringify`
writes), strings scanned 8 bytes at a time, and an object of a type of
one member only checked for the fields it must have (each stored value was
checked). Anything else (invalid JSON, a value a field cannot hold, a
16-bit or concatenated string) goes the general way, so that the result
and the errors are always the same as `JSON.parse` and the conversion's
(the differential tests run both: `aot_json_parse.ts`). For `r.json()`, a
host may parse the body itself (`lum_set_json_hook`; Beyond parses a
`Request`'s or `Response`'s bytes, `lum_json_parse_utf8_as`), else the
result of `r.json()` is converted. A small object (6 fields, one of them
a nested object) parses in 329–350 ns, as bun's `JSON.parse` (343–381
ns), and 50 objects with fields the type does not have in 31–32 µs,
bun's 45–46 µs; about as fast as the plain parse since `0024` (Beyond's
docs/performance.md).

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

./target/release/luminous run bench/fib.ts
./target/release/luminous build bench/mandelbrot.ts -o mandelbrot
./target/release/luminous build --backend llvm bench/nbody_aot.ts -o nbody
./target/release/luminous report tests/cases/rejected.ts
./target/release/luminous emit-js bench/fib.ts       # what the interpreter runs
./target/release/luminous ir bench/fib.ts            # typed IR of native functions
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

### Against node and bun

Median wall time of 7 interleaved runs (the program's own `elapsed`) and
peak memory (maximum resident set size, of each program alone: measured
from a small launcher, as a child forked from a larger process counts
that one's pages too), LLVM backend with `--march x86-64-v3`, against the
same program's erased JavaScript under node 22.22 and bun 1.4.3 (with
`BUN_OPTIONS` cleared: this machine's environment sets it to `--smol`),
on a 4-core x86-64 cloud VM. Timings there drift by 10–20% between runs,
and more over a day (this run was 40 to 60% slower than the fastest of
the same day, for every runtime alike); memory is stable.

| benchmark    | Luminous         | node             | bun              | time / bun | memory / bun |
|--------------|-----------------:|-----------------:|-----------------:|-----------:|-------------:|
| hello world  |    –  / 5.7 MB   |     –  / 43 MB   |    –  / 12 MB    |          – |        0.46× |
| fib          |   9 ms / 5.8 MB  |   39 ms / 48 MB  |   19 ms / 19 MB  |      0.47× |        0.30× |
| binary_trees | 265 ms / 57 MB   |  359 ms / 96 MB  |  412 ms / 55 MB  |      0.64× |        1.03× |
| xorshift     |  54 ms / 5.7 MB  |  133 ms / 49 MB  |   79 ms / 21 MB  |      0.68× |        0.27× |
| tokenizer    | 190 ms / 87 MB   |  579 ms / 210 MB |  266 ms / 193 MB |      0.71× |        0.45× |
| records      | 463 ms / 54 MB   | 1025 ms / 132 MB |  601 ms / 64 MB  |      0.77× |        0.85× |
| raytracer    |  31 ms / 10 MB   |   61 ms / 52 MB  |   40 ms / 38 MB  |      0.78× |        0.27× |
| mandelbrot   |  24 ms / 5.7 MB  |   42 ms / 50 MB  |   30 ms / 22 MB  |      0.80× |        0.27× |
| integrate    | 205 ms / 5.9 MB  |  384 ms / 50 MB  |  242 ms / 22 MB  |      0.85× |        0.26× |
| objects      | 379 ms / 64 MB   | 1063 ms / 184 MB |  442 ms / 88 MB  |      0.86× |        0.73× |
| arrays       | 116 ms / 34 MB   |  176 ms / 80 MB  |  135 ms / 78 MB  |      0.86× |        0.43× |
| heap         | 272 ms / 132 MB  |  483 ms / 169 MB |  284 ms / 131 MB |      0.96× |        1.01× |
| nbody        | 176 ms / 6.0 MB  |  143 ms / 51 MB  |  169 ms / 24 MB  |      1.04× |        0.25× |
| primes       | 205 ms / 5.8 MB  |  239 ms / 49 MB  |  189 ms / 21 MB  |      1.08× |        0.27× |
| logparser    | 1106 ms / 175 MB | 1383 ms / 250 MB |  970 ms / 223 MB |      1.14× |        0.79× |
| shapes       | 143 ms / 21 MB   |  241 ms / 100 MB |  114 ms / 45 MB  |      1.25× |        0.48× |

`bun --smol` changes little here but for raytracer (29 MB, 57 ms) and
arrays (65 MB). The collector can be held to a smaller young generation
(`QJS_GC_YOUNG_MAX`, in KB): at 2MB, objects peaks at 45MB and tokenizer
at 107MB, taking two to four times as long.

## Roadmap

- **`int` fields and elements**: `{ value: int }`, `int[]` (checked reads),
  and `int` parameters of closures.
- **Modules**: dynamic `import()` of the program's modules (evaluated when
  first imported), `import.meta`, CommonJS packages, `tsconfig.json`
  `paths`.
- **Position-preserving type stripping**, so interpreter stack traces point
  at the original `.ts` lines.

### Open performance work

Five open-ended lines of work, each left at a finished, tested point, with
what would come next:

- **LLVM code quality.** Most of the benchmarks above already run faster
  than bun; what remains is in code the compiler cannot specialize yet.
  Method calls on receivers of several classes (`for (const s of shapes)
  s.area()`) now dispatch on the method the receiver reaches and run each
  class's method inline, with no runtime lookups left in `bench/shapes.ts`'s
  loop (1.25 to 1.6× bun's time, from run to run). Next: keep numbers unboxed across such
  calls and their returns, and drop the method check where no class of the
  hierarchy can be changed at run time.
- **Engine fast paths: allocation, strings, builtins.** Objects, class
  instances and arrays are allocated inline by native code; `+` on strings,
  template literals and `+` chains build their result with one engine call
  (int32s written as digits), regular expression literals are compiled
  once and searched from their first character, `Map`/`Set` keys hash and
  compare cheaply, and `+=` onto a long string appends in place, so
  `bench/maps.ts` and `bench/strbuild.ts` run faster than under bun.
  Next: `Map`/`Set` entries in an open-addressing table rather than linked
  records (fewer pointer chases, smaller entries), substrings that share
  their parent's characters at shorter lengths (most of
  `bench/logparser.ts`'s slices are copies), and native paths for the
  other builtins that profiles of real programs show.
- **Float code (nbody, raytracer).** Speculative regions read and write
  `number` fields as doubles, and non-escaping class instances become
  locals: raytracer runs faster than bun, nbody about as fast (its loads follow
  a decoded reference, see [Values](#values-8-bytes)). Next: scalar replacement through
  calls that are not inlined (a `Vec` passed to and returned from a
  method), and SIMD for the pair loop of nbody-like code.
- **Numeric regions in methods.** Regions cover `this` fields in methods
  and the bodies of inlined methods. Next: regions that span calls to
  small methods of other objects (predicted and guarded), and regions over
  loops that call such methods.
- **Garbage collector.** The engine's reference counting is replaced by a
  tracing, non-moving mark-sweep collector with generations (sticky mark
  bits and a write barrier), conservative stack roots and inline bump
  allocation. Next: parallel marking, lazy (incremental) sweeping, and a
  copying young generation, which needs the objects that conservative
  roots reach to be pinned (as mostly-copying collectors do).

## Layout

```
crates/luminous-front    oxc front end: module graph (bundle.rs), checker, typed IR, type erasure, `aot` marker
crates/luminous-codegen  lowering to a backend-neutral SSA IR (mir.rs); Cranelift (clif.rs) and LLVM (llvm.rs) backends
crates/luminous-cli      `luminous` binary: glue generation, qjsc + clang + cc driver; differential tests
runtime/              C runtime linked into every program (helpers, boxes, async frames, cells)
vendor/quickjs        QuickJS-NG (git submodule, kept pristine)
patches/              engine patch applied by scripts/build-quickjs.sh
tests/cases           differential test programs
tests/errors          programs that must fail to compile, with expected diagnostics
bench/                benchmarks + bench/run.sh; bench/micro/ for microbenchmarks
```
