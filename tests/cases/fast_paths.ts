// @native ownAccessors linkedCalls deep literals numArith closureShapes valueCaptures captureTdz switchCapture arrayIter giveUp promises nativeA nativeB patched
// @interpreted slowAdd interpC
// Fast paths: own accessor caches, linked calls, object literal and
// closure shapes, value captures, numeric arithmetic on JS values.

function ownAccessors(): string {
  const log: unknown[] = [];
  const o = {
    x: 1,
    get v(): number {
      return this.x * 10;
    },
    set v(n: number) {
      this.x = n;
    },
  };
  let acc = 0;
  for (let i = 0; i < 50; i++) {
    o.v = i;
    acc += o.v;
    if (i === 20) {
      // The getter replaced in place (no shape change).
      Object.defineProperty(o, "v", { get: () => -1 });
    }
    if (i === 30) {
      // The setter removed in place: writes throw in strict code.
      Object.defineProperty(o, "v", { set: undefined });
    }
    if (i === 40) {
      Object.defineProperty(o, "v", { get: undefined });
    }
    if (i >= 30) {
      try {
        o.v = 5;
      } catch (e) {
        log.push((e as Error).constructor.name);
        break;
      }
    }
  }
  const p = {
    get only(): string {
      return "g";
    },
  };
  for (let i = 0; i < 3; i++) log.push(p.only);
  Object.defineProperty(p, "only", { value: "data", writable: true });
  log.push(p.only);
  return acc + "," + log.join(",") + "," + o.x;
}

interpreted function slowAdd(a: number, b: number): number {
  return a + b;
}

class Plain {
  n = 1;
}

function linkedCalls(): string {
  const out: unknown[] = [];
  const o = {
    k: 2,
    m(x: number): number {
      return x * this.k;
    },
    s(x: string): string {
      return x + this.k;
    },
  };
  const fs: ((a: number, b: number) => number)[] = [
    (a, b) => a - b,
    slowAdd,
    Math.max,
    slowAdd.bind(null, 100),
    function (a: number) {
      return a * 3;
    },
  ];
  for (let i = 0; i < 6; i++) {
    out.push(o.m(i));
    for (const f of fs) out.push(f(i, 7));
  }
  // A native entry that declines (an ill-typed argument): the fallback.
  const m = o.m as unknown as (x: unknown) => unknown;
  out.push(m.call(o, "4"), m.call(o, undefined), o.s("z"));
  try {
    (Plain as unknown as () => void)();
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  return out.join(",");
}

function deep(): string {
  const r = {
    depth: 0,
    down(n: number): number {
      this.depth = n;
      return this.down(n + 1);
    },
  };
  try {
    r.down(0);
  } catch (e) {
    return (e as Error).constructor.name;
  }
  return "no overflow";
}

function literals(): string {
  const out: unknown[] = [];
  const made: object[] = [];
  let n = 0;
  const f = (i: number): number => {
    if (i % 3 === 2) throw new Error("e" + i);
    return i * 2;
  };
  for (let i = 0; i < 9; i++) {
    try {
      made.push({ a: i, b: f(i), c: "s" + i });
    } catch (e) {
      out.push((e as Error).message);
    }
    made.push({ 1: "one", b: i, 0: "zero" });
    made.push({});
    const dup = { a: 1, b: 2, a2: 3 } as Record<string, number>;
    dup.extra = i;
    n += Object.keys(dup).length;
  }
  for (const m of made) out.push(JSON.stringify(m) + Object.keys(m).join(""));
  (made[0] as Record<string, unknown>).added = true;
  out.push(JSON.stringify(made[0]), JSON.stringify(made[3]), n);
  return out.join(";");
}

function numArith(): string {
  const log: string[] = [];
  const vals: unknown[] = [3, 2.5, "4", "x", null, undefined, true, -0, [7], { valueOf: () => 9 }];
  const out: unknown[] = [];
  for (const v of vals) {
    const a = v as number;
    out.push(a * 2, a - 1, 10 / a, a % 3, a ** 2, 1 - a);
  }
  const ordered = {
    valueOf(): number {
      log.push("valueOf");
      return 6;
    },
  } as unknown as number;
  const g = (): number => {
    log.push("g");
    return 3;
  };
  out.push(ordered * g(), g() - ordered);
  for (const bad of [BigInt(10), Symbol("s")] as unknown[]) {
    try {
      out.push((bad as number) * 2);
    } catch (e) {
      out.push((e as Error).constructor.name + ":" + (e as Error).message);
    }
  }
  out.push(Object.is((-0 as unknown as number) * 1, -0));
  return out.join(",") + "|" + log.join(",");
}

function closureShapes(): string {
  const out: unknown[] = [];
  const fs: ((x: number) => number)[] = [];
  const as: (() => Promise<number>)[] = [];
  for (let i = 0; i < 5; i++) {
    const k = i * 10;
    fs.push((x) => x + k);
    as.push(async () => k);
  }
  Object.defineProperty(fs[1], "name", { value: "renamed" });
  (fs[2] as unknown as Record<string, unknown>).tag = 1;
  for (const f of fs) out.push(f(1), f.name, f.length, Object.keys(f).join(""));
  out.push(Object.getPrototypeOf(as[0]) === Object.getPrototypeOf(async () => 0), as[3].name);
  const named = function inner(a: number, b: number) {
    return a + b;
  };
  out.push(named.name, named.length);
  return out.join(",");
}

function valueCaptures(): string {
  const out: unknown[] = [];
  const getters: (() => number)[] = [];
  for (let i = 0; i < 4; i++) {
    const c = i * i;
    let neverChanged = c + 1;
    let changed = c + 2;
    const g = () => c + neverChanged + changed;
    changed = 100;
    getters.push(g);
    // Nested closures pass the value along.
    const outer = () => () => c * 2;
    out.push(outer()());
  }
  for (const g of getters) out.push(g());
  const asyncCaps: Promise<number>[] = [];
  for (let i = 0; i < 2; i++) {
    const c = i + 5;
    asyncCaps.push((async () => c)());
  }
  return out.join(",");
}

function captureTdz(): string {
  const out: unknown[] = [];
  try {
    early();
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  const late = 7;
  function early(): number {
    return late;
  }
  out.push(early());
  const before = () => afterDecl;
  try {
    before();
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  const afterDecl = 3;
  out.push(before());
  return out.join(",");
}

function switchCapture(k: number): string {
  const out: unknown[] = [];
  switch (k) {
    case 0:
      const v = 1;
      out.push((() => v)());
    // falls through
    case 1:
      try {
        out.push((() => v)());
      } catch (e) {
        out.push((e as Error).constructor.name);
      }
  }
  return out.join(",");
}

function arrayIter(): string {
  const out: number[] = [];
  const xs = [1, 2, 3];
  for (const x of xs) out.push(x);
  const original = Array.prototype[Symbol.iterator];
  Array.prototype[Symbol.iterator] = function* () {
    yield 42;
  };
  for (const x of xs) out.push(x);
  Array.prototype[Symbol.iterator] = original;
  for (const x of xs) out.push(x);
  return out.join(",");
}

function giveUp(): string {
  // Property reads on objects of unshared shapes (here after a `delete`)
  // never cache.
  let s = 0;
  const o: { gone?: number; b: number; f: (x: number) => number } = { gone: 1, b: 2, f: (x) => x + 1 };
  delete o.gone;
  for (let i = 0; i < 200; i++) {
    if (i === 150) {
      o.b = 1000;
      o.f = (x) => x * 1000;
    }
    s += o.b + o.f(1);
  }
  return String(s);
}

console.log(ownAccessors());
console.log(linkedCalls());
console.log(deep());
console.log(literals());
console.log(numArith());
console.log(closureShapes());
console.log(valueCaptures());
console.log(captureTdz());
console.log(switchCapture(0), switchCapture(1));
console.log(arrayIter());
console.log(giveUp());

// Promises: settled fast paths, identity and observable lookups.
class P<T> extends Promise<T> {}
async function promises(): Promise<string> {
  const log: unknown[] = [];
  const p1 = Promise.resolve(1);
  log.push(Promise.resolve(p1) === p1);
  const sub = P.resolve(2);
  log.push(sub instanceof P, Promise.resolve(sub) === (sub as Promise<number>));
  const watched = Promise.resolve(3);
  Object.defineProperty(watched, "constructor", {
    get() {
      log.push("constructor read");
      return Promise;
    },
  });
  log.push(Promise.resolve(watched) === watched);
  log.push(await Promise.resolve(4));
  try {
    await Promise.reject(5);
  } catch (e) {
    log.push("rejected " + e);
  }
  const thenable = {
    then(res: (v: number) => void) {
      log.push("then called");
      res(6);
    },
  };
  log.push(await thenable);
  let ticks = 0;
  const counter = (async () => {
    for (let i = 0; i < 5; i++) {
      ticks++;
      await null;
    }
  })();
  for (let i = 0; i < 3; i++) {
    await Promise.resolve(i);
    log.push("tick " + ticks);
  }
  await counter;
  const order: string[] = [];
  const a = Promise.resolve().then(() => order.push("then1"));
  await Promise.resolve(0);
  order.push("after await");
  await a;
  log.push(order.join(" "));
  return log.join(",");
}
promises().then((s) => console.log(s));

// Microtask order with awaits of primitives and promises, native and
// interpreted, against `.then` chains; a replaced Promise.resolve is called.
const order: string[] = [];
async function nativeA(): Promise<void> {
  for (let i = 0; i < 3; i++) {
    order.push("A" + i);
    await i;
  }
}
async function nativeB(): Promise<void> {
  for (let i = 0; i < 3; i++) {
    order.push("B" + i);
    await Promise.resolve(i);
  }
}
interpreted async function interpC(): Promise<void> {
  for (let i = 0; i < 3; i++) {
    order.push("C" + i);
    await i;
  }
}
async function patched(): Promise<string> {
  const original = Promise.resolve;
  let calls = 0;
  Promise.resolve = ((v: unknown) => {
    calls++;
    return original.call(Promise, v);
  }) as typeof Promise.resolve;
  const v = await Promise.resolve(7);
  Promise.resolve = original;
  const w = await Promise.resolve(8);
  return [v, w, calls].join(",");
}
Promise.resolve()
  .then(() => {
    const all = Promise.all([nativeA(), nativeB(), interpC()]);
    Promise.resolve()
      .then(() => order.push("t1"))
      .then(() => order.push("t2"))
      .then(() => order.push("t3"));
    return all;
  })
  .then(() => patched())
  .then((s) => console.log(order.join(" ") + " | " + s));
