// @native count withFinally delegate nums sum early chain rejects inner forwarding noThrow items ticks protocolEdges main
const log: string[] = [];
async function* count(n: number) {
  for (let i = 0; i < n; i++) {
    await null;
    yield i;
  }
  return "done";
}
async function* withFinally() {
  try {
    yield 1;
    yield 2;
  } finally {
    console.log("cleanup");
    await Promise.resolve();
    console.log("cleanup awaited");
  }
}
async function* delegate() {
  const r = yield* count(2);
  console.log("inner returned", r);
  yield* [10, 20];
  return 99;
}
async function* nums(n: number) {
  try {
    for (let i = 0; i < n; i++) {
      await null;
      yield i;
    }
  } finally {
    log.push("nums closed");
  }
}
async function sum(): Promise<number> {
  let s = 0;
  for await (const x of nums(4)) s += x;
  for await (const x of [1, Promise.resolve(2), 3]) s += x as number;
  return s;
}
async function early(): Promise<string> {
  for await (const x of nums(10)) {
    if (x === 2) break;
  }
  try {
    for await (const x of nums(10)) {
      if (x === 1) throw new Error("body threw");
    }
  } catch (e) {
    log.push((e as Error).message);
  }
  const first = await (async () => {
    for await (const x of nums(10)) return x + 100;
    return -1;
  })();
  return log.splice(0).join(" | ") + " | " + first;
}
async function* chain() {
  for await (const x of nums(3)) yield x * 10;
}
async function* rejects() {
  try {
    yield Promise.reject(new Error("rejected yield"));
  } catch (e) {
    yield "caught: " + (e as Error).message;
  }
}
async function* inner() {
  try {
    yield "a";
    yield "b";
  } catch (e) {
    yield "inner caught " + e;
  } finally {
    log.push("inner finally");
  }
}
async function* forwarding() {
  const r = yield* inner();
  return r;
}
async function* noThrow() {
  const it = {
    [Symbol.asyncIterator]: () => it,
    next: () => Promise.resolve({ done: false, value: "x" }),
    return: () => {
      log.push("closed");
      return Promise.resolve({ done: true, value: undefined });
    },
  } as unknown as AsyncIterable<string>;
  yield* it;
}
class Store {
  constructor(public data: number[]) {}
  async *items() {
    for (const d of this.data) yield d * 2;
  }
}
async function ticks(): Promise<string> {
  const order: string[] = [];
  const g = count(3);
  const p = g.next().then((r) => order.push("gen " + r.value));
  Promise.resolve().then(() => order.push("tick 1")).then(() => order.push("tick 2")).then(() => order.push("tick 3"));
  await p;
  await g.next().then((r) => order.push("gen " + r.value));
  await Promise.resolve();
  await Promise.resolve();
  return order.join(", ");
}
async function protocolEdges(): Promise<string> {
  const out: string[] = [];
  const show = (r: IteratorResult<unknown>) => JSON.stringify(r);
  const a = count(2);
  out.push(show(await a.return(Promise.resolve("early"))), show(await a.next()));
  const b = count(2);
  try {
    await b.throw(new Error("before start"));
  } catch (e) {
    out.push("threw " + (e as Error).message);
  }
  out.push(show(await b.next()));
  const c = count(3);
  await c.next();
  out.push(show(await c.return(Promise.resolve(7))), show(await c.next()));
  const r = rejects();
  out.push(show(await r.next()), show(await r.next()));
  const f = forwarding();
  out.push(show(await f.next()), show(await f.throw("E")), show(await f.next()));
  const f2 = forwarding();
  await f2.next();
  out.push(show(await f2.return("R")));
  const n = noThrow();
  await n.next();
  try {
    await n.throw("T");
  } catch (e) {
    out.push((e as Error).message);
  }
  const s: number[] = [];
  for await (const x of new Store([1, 2, 3]).items()) s.push(x);
  out.push(s.join("+"));
  return out.join("\n") + "\n" + log.splice(0).join(" | ");
}
async function main(): Promise<void> {
  const g = count(3);
  console.log(JSON.stringify(await g.next()), JSON.stringify(await g.next()));
  console.log(JSON.stringify(await g.next()), JSON.stringify(await g.next()), JSON.stringify(await g.next()));
  const w = withFinally();
  console.log(JSON.stringify(await w.next()));
  console.log(JSON.stringify(await w.return(42)));
  const d = delegate();
  const out: unknown[] = [];
  for (let r = await d.next(); !r.done; r = await d.next()) out.push(r.value);
  console.log(out.join(","));
  const q = count(2);
  const ps = [q.next(), q.next(), q.next()];
  console.log((await Promise.all(ps)).map((x) => JSON.stringify(x)).join(" "));
  const thrower = (async function* () {
    try {
      yield 1;
    } catch (e) {
      yield "caught " + e;
    }
  })();
  await thrower.next();
  console.log(JSON.stringify(await thrower.throw("boom")));
  console.log(await sum(), log.splice(0).join(" | "));
  console.log(await early());
  const got: number[] = [];
  for await (const x of chain()) got.push(x);
  console.log(got.join(","), log.splice(0).join(" | "));
  console.log(await ticks());
  console.log(await protocolEdges());
}
main();
