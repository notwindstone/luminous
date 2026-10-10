// @native decls params assigns spreads loops closing forIn order restArgs callRest
const log: string[] = [];

function decls(o: any, a: any): string {
  const { x, y: why = "dy", ["z" + 1]: z1, ...others } = o;
  const [p, , q = "dq", ...tail] = a;
  let { n: { m } = { m: "dm" } } = o;
  var [[deep]] = [[7]];
  const { length } = "hello";
  return JSON.stringify({ x, why, z1, others, p, q, tail, m, deep, length });
}

function params({ a, b = 2 }: any, [c, ...ds]: any = [9], ...rest: any[]): string {
  return JSON.stringify({ a, b, c, ds, rest });
}

function assigns(): string {
  let a = 1;
  let b = 2;
  [a, b] = [b, a];
  const o: any = {};
  ({ x: o.x, y: o["y"] = 5, ...o.rest } = { x: 1, z: 3, w: 4 });
  let r: any;
  const v = ([a, ...r] = "xyz");
  return JSON.stringify({ a, b, o, r, v });
}

function spreads(xs: number[], o: any): string {
  const arr = [0, ...xs, ...new Set([4, 4, 5]), , 6];
  const obj = { a: 1, ...o, ["k" + 2]: 2, ...null, ...undefined, b: 3 };
  const m = Math.max(...xs, 1);
  const s = String.fromCharCode(...[104, 105]);
  const d = new Date(...[2020, 1, 3]).getDate();
  return JSON.stringify({ arr, len: arr.length, hole: 6 in arr, obj, m, s, d });
}

function loops(xs: any[], m: Map<string, number>): string {
  const out: unknown[] = [];
  for (const x of xs) out.push(x);
  for (const [k, v] of m) out.push(k + "=" + v);
  for (const ch of "héllo") out.push(ch);
  let i = 0;
  for (i of [10, 20]) out.push(i);
  const fns: (() => unknown)[] = [];
  for (const x of xs) fns.push(() => x);
  for (let k = 0; k < fns.length; k++) out.push(fns[k]());
  outer: for (const a of [1, 2, 3]) {
    for (const b of [1, 2, 3]) {
      if (b === 2) continue outer;
      if (a === 3) break outer;
      out.push(a * 10 + b);
    }
  }
  return out.join(",");
}

// An iterator that records how it is closed.
function counter(n: number, name: string) {
  let i = 0;
  return {
    [Symbol.iterator]() {
      return this;
    },
    next() {
      log.push(`${name}.next`);
      return i < n ? { value: i++, done: false } : { value: undefined, done: true };
    },
    return() {
      log.push(`${name}.return`);
      return {};
    },
  };
}

function closing(): string {
  for (const x of counter(5, "brk")) if (x === 1) break;
  for (const x of counter(2, "all")) void x;
  try {
    for (const x of counter(5, "thr")) if (x === 1) throw new Error("boom");
  } catch (e) {
    log.push((e as Error).message);
  }
  const [a] = counter(3, "pat");
  const [b, c, d] = counter(2, "short");
  for (const x of counter(3, "cont")) {
    if (x === 0) continue;
    log.push("x" + x);
  }
  const f = () => {
    for (const x of counter(3, "ret")) return x;
  };
  f();
  try {
    const [e = (() => { throw new Error("default") })()] = counter(0, "none");
    void e;
  } catch (err) {
    log.push((err as Error).message);
  }
  return [a, b, c, d].join(",");
}

function forIn(o: any): string {
  const keys: string[] = [];
  for (const k in o) keys.push(k);
  for (const k in [5, 6]) keys.push("i" + k);
  for (const k in null) keys.push("never");
  const proto = { inherited: 1 };
  const child = Object.create(proto);
  child.own = 2;
  for (const k in child) keys.push(k);
  const del: any = { a: 1, b: 2, c: 3 };
  for (const k in del) {
    delete del.b;
    keys.push(k);
  }
  return keys.join(",");
}

// Targets are evaluated before the values are read.
function fixtures(steps: string[]) {
  const obj = { get target() { steps.push("target"); return {}; } };
  const src = { get v() { steps.push("read"); return 1; } };
  const it = { [Symbol.iterator]() { return { next() { steps.push("next"); return { done: false, value: 1 }; }, return() { steps.push("return"); return {}; } }; } };
  return { obj, src, it };
}

function order(): string {
  const steps: string[] = [];
  const { obj, src, it } = fixtures(steps);
  ({ v: obj.target.x } = src);
  [obj.target.y] = it;
  return steps.join(",");
}

function restArgs(a: number, ...rest: number[]): number {
  let s = a;
  for (const r of rest) s += r;
  return s + rest.length * 1000;
}

function callRest(): string {
  return [restArgs(1), restArgs(1, 2, 3), restArgs(...[4, 5])].join(",");
}

console.log(decls({ x: 1, z1: 3, extra: 4, n: { m: 5 } }, [1, 2, undefined, 4, 5]));
console.log(decls({ y: undefined }, "ab"));
for (const bad of [null, undefined]) {
  try {
    decls(bad, []);
  } catch (e) {
    console.log("decls:", (e as Error).name);
  }
}
try {
  decls({}, 5);
} catch (e) {
  console.log("decls:", (e as Error).name, (e as Error).message);
}
console.log(params({ a: 1 }), params({ a: 1, b: 0 }, [1, 2, 3], 4, 5));
console.log(assigns());
console.log(spreads([1, 2, 3], { b: 9, c: 10 }));
console.log(loops([1, "two", { three: 3 }], new Map([["a", 1], ["b", 2]])));
console.log(closing());
console.log(log.join(" "));
console.log(forIn({ x: 1, y: 2, 3: "three" }));
console.log(order());
console.log(callRest(), restArgs.length, params.length);
try {
  for (const x of 5 as any) void x;
} catch (e) {
  console.log((e as Error).name, (e as Error).message);
}
