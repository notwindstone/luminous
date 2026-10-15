// @native main awaitAll chains settle thenables subclass ticks dying combinators
// The order in which promise reactions, awaits and async functions run
// (the microtask queue), and promises' states: as the spec has them.
import { setTimeout } from "qjs:os";
const log: string[] = [];

async function inner(x: number): Promise<number> {
  log.push("inner " + x);
  return x;
}

async function throws(x: number): Promise<number> {
  log.push("throws " + x);
  throw new Error("e" + x);
}

async function chains(): Promise<void> {
  const p = Promise.resolve(1);
  p.then((v) => log.push("then a " + v));
  p.then((v) => log.push("then b " + v)).then(() => log.push("then b2"));
  const v = await p;
  log.push("await p " + v);
  const w = await inner(2);
  log.push("await inner " + w);
  try {
    await throws(3);
  } catch (e) {
    log.push("caught " + (e as Error).message);
  }
  await null;
  log.push("after null");
  const r = await Promise.reject(new Error("r")).catch((e: Error) => "handled " + e.message);
  log.push(r);
  await Promise.resolve(4).finally(() => log.push("finally"));
  log.push("after finally");
}

async function thenables(): Promise<void> {
  const t = { then(res: (v: number) => void) { log.push("thenable then"); res(5); } };
  const v = await t;
  log.push("await thenable " + v);
  const getterThen = {
    get then() {
      log.push("then getter");
      return undefined;
    },
  };
  const g = await getterThen;
  log.push("await getterThen " + (g === getterThen));
  // An async function returning a promise: two more ticks.
  const ret = async () => Promise.resolve(6);
  ret().then((x) => log.push("returned promise " + x));
  Promise.resolve().then(() => log.push("tick 1")).then(() => log.push("tick 2")).then(() => log.push("tick 3"));
}

async function awaitAll(): Promise<void> {
  const all = await Promise.all([inner(10), Promise.resolve(11), 12]);
  log.push("all " + all.join(","));
  const settled = await Promise.allSettled([inner(13), throws(14)]);
  log.push("allSettled " + settled.map((s) => s.status).join(","));
  const race = await Promise.race([new Promise((r) => setTimeout(() => r("slow"), 5)), inner(15)]);
  log.push("race " + race);
  const any = await Promise.any([throws(16), inner(17)]);
  log.push("any " + any);
  try {
    await Promise.any([throws(18)]);
  } catch (e) {
    log.push("any rejected " + (e as AggregateError).errors.length);
  }
}

class MyPromise<T> extends Promise<T> {
  tag = "mine";
}

async function subclass(): Promise<void> {
  const m = new MyPromise<number>((r) => r(20));
  log.push("subclass " + (m instanceof MyPromise) + " " + (m as any).tag);
  const t = m.then((x) => x + 1);
  log.push("then of subclass " + (t instanceof MyPromise));
  log.push("await subclass " + (await t));
  // Properties on a promise, past what its cell holds.
  const p: any = Promise.resolve(21);
  p.a = 1;
  p.b = 2;
  p.c = 3;
  p.d = 4;
  log.push("props " + (await p) + " " + Object.keys(p).join(""));
}

// Awaits in a row, interleaved with another chain: each await one tick.
async function ticks(name: string, n: number): Promise<void> {
  for (let i = 0; i < n; i++) {
    log.push(name + i);
    await undefined;
  }
}

// Promises that die pending with reactions (their records freed), many of
// them, while collections run.
function dying(n: number): number {
  let count = 0;
  for (let i = 0; i < n; i++) {
    const p = new Promise<number>(() => {});
    p.then(() => count++);
    p.then(() => count++, () => count--);
    const q = Promise.resolve(i);
    q.then(() => count++);
  }
  return count;
}

async function settle(): Promise<void> {
  let resolveIt!: (v: number) => void;
  const p = new Promise<number>((r) => (resolveIt = r));
  const waiting = (async () => log.push("waited " + (await p)))();
  p.then((v) => log.push("then of pending " + v));
  log.push("before resolve");
  resolveIt(30);
  resolveIt(31);
  await waiting;
  let rejectIt!: (e: Error) => void;
  const q = new Promise<number>((_, j) => (rejectIt = j));
  const caught = (async () => {
    try {
      await q;
    } catch (e) {
      log.push("rejected await " + (e as Error).message);
    }
  })();
  rejectIt(new Error("late"));
  await caught;
}

// The combinators over arrays of promises and primitives, interleaved
// with other reactions, and when what they rely on is changed.
async function combinators(): Promise<void> {
  const order: string[] = [];
  const later = (v: number) => new Promise<number>((r) => setTimeout(() => r(v), 1));
  const tick = Promise.resolve().then(() => order.push("t1")).then(() => order.push("t2")).then(() => order.push("t3"));
  const a = Promise.all([inner(1), 2, Promise.resolve(3)]).then((v) => order.push("all " + v.join(",")));
  const b = Promise.allSettled([throws(4), 5, later(6)]).then((v) =>
    order.push("allSettled " + v.map((x) => x.status + ":" + ("value" in x ? x.value : (x.reason as Error).message)).join(",")),
  );
  const c = Promise.any([throws(7), later(8), 9]).then((v) => order.push("any " + v));
  const d = Promise.race([later(10), 11, inner(12)]).then((v) => order.push("race " + v));
  const e = Promise.all([inner(13), throws(14), throws(15)]).catch((x: Error) => order.push("all rejected " + x.message));
  const f = Promise.all([]).then((v) => order.push("empty all " + v.length));
  const g = Promise.any([]).catch((x: AggregateError) => order.push("empty any " + x.errors.length));
  const h = Promise.allSettled([]).then((v) => order.push("empty allSettled " + v.length));
  const i = Promise.any([throws(16), Promise.reject(new Error("r17"))]).catch((x: AggregateError) =>
    order.push("any none " + x.errors.map((y: Error) => y.message).join(",")),
  );
  const thenable = { then(r: (v: number) => void) { order.push("thenable then"); r(18); } };
  const j = Promise.all([thenable, 19]).then((v) => order.push("with thenable " + v.join(",")));
  const sparse: number[] = [1];
  sparse[3] = 4;
  const k = Promise.all(sparse).then((v) => order.push("sparse " + v.length + " " + v[1]));
  await Promise.all([tick, a, b, c, d, e, f, g, h, i, j, k]);
  log.push(order.join(" | "));
}

async function patched(): Promise<void> {
  const order: string[] = [];
  const then = Promise.prototype.then;
  Promise.prototype.then = function (this: Promise<unknown>, ...args: any[]) {
    order.push("patched then");
    return then.apply(this, args as any);
  } as any;
  await Promise.all([inner(20), 21]).then((v) => order.push("all " + v.join(",")));
  Promise.prototype.then = then;
  const iter = Array.prototype[Symbol.iterator];
  Array.prototype[Symbol.iterator] = function (this: unknown[]) {
    order.push("patched iterator");
    return iter.call(this);
  } as any;
  await Promise.race([inner(22), 23]).then((v) => order.push("race " + v));
  Array.prototype[Symbol.iterator] = iter;
  const resolve = Promise.resolve;
  (Promise as any).resolve = function (this: PromiseConstructor, v: unknown) {
    order.push("patched resolve");
    return resolve.call(this, v);
  };
  await Promise.allSettled([inner(24)]).then((v) => order.push("allSettled " + v.length));
  (Promise as any).resolve = resolve;
  const p = Promise.resolve(25);
  (p as any).constructor = undefined;
  await p.then((v) => order.push("no constructor " + v));
  class Sub<T> extends Promise<T> {}
  const s = Sub.all([inner(26)]);
  order.push("Sub.all " + (s instanceof Sub));
  await s;
  await Promise.all([inner(27)]).then((v) => order.push("restored " + v));
  log.push(order.join(" | "));
}

async function main(): Promise<void> {
  const a = chains();
  const b = ticks("x", 4);
  const c = ticks("y", 3);
  await Promise.all([a, b, c]);
  await thenables();
  await new Promise((r) => setTimeout(r, 0));
  await awaitAll();
  await subclass();
  await settle();
  await combinators();
  await patched();
  log.push("dying " + dying(3000));
  await new Promise((r) => setTimeout(r, 0));
  for (const line of log) console.log(line);
}

main().catch((e) => console.log("failed", e));
