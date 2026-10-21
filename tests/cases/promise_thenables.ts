// @native returnsPromise throwsLater flush main
// A promise resolved with a promise of %Promise% takes it over in a job of
// its own (the spec's NewPromiseResolveThenableJob), then reacts to it: in
// the same order of jobs whether the engine calls `then` or reacts
// natively; and a `then` that is not the builtin, or a promise whose
// constructor or species is another, is still called.
const log: string[] = [];
async function returnsPromise(p: Promise<string>): Promise<string> {
  return p;
}
async function throwsLater(): Promise<string> {
  return Promise.reject(new Error("later"));
}
class MyPromise<T> extends Promise<T> {}
async function flush(): Promise<void> {
  for (let i = 0; i < 30; i++) await null;
}
async function main() {
  // Order against other microtasks.
  const a = returnsPromise(Promise.resolve("a"));
  a.then((v) => log.push("a:" + v));
  Promise.resolve().then(() => log.push("tick1")).then(() => log.push("tick2")).then(() => log.push("tick3")).then(() => log.push("tick4"));
  new Promise<string>((r) => r(Promise.resolve("b"))).then((v) => log.push("b:" + v));
  throwsLater().catch((e) => log.push("rejected:" + e.message));
  // A pending one, settled later.
  let settle!: (v: string) => void;
  const pending = new Promise<string>((r) => (settle = r));
  returnsPromise(pending).then((v) => log.push("pending:" + v));
  await flush();
  settle("late");
  await flush();
  console.log(log.join(" "));
  log.length = 0;
  // A subclass: its then is still called.
  const m = MyPromise.resolve("m");
  returnsPromise(m as any).then((v) => log.push("sub:" + v));
  // An own then.
  const own = Promise.resolve("own") as any;
  own.then = function (this: any, f: any, r: any) {
    log.push("own then called");
    return Promise.prototype.then.call(this, f, r);
  };
  returnsPromise(own).then((v) => log.push("own:" + v));
  // A thenable object.
  returnsPromise({ then(f: any) { log.push("thenable called"); f("obj"); } } as any).then((v) => log.push("obj:" + v));
  await flush();
  console.log(log.join(" "));
  log.length = 0;
  // Promise.prototype.then replaced: the replacement is called.
  const orig = Promise.prototype.then;
  (Promise.prototype as any).then = function (this: any, f: any, r: any) {
    log.push("patched then");
    return orig.call(this, f, r);
  };
  const q = returnsPromise(Promise.resolve("q"));
  (Promise.prototype as any).then = orig;
  q.then((v) => log.push("q:" + v));
  await flush();
  console.log(log.join(" "));
  // Species changed: the species constructor is consulted.
  log.length = 0;
  const desc = Object.getOwnPropertyDescriptor(Promise, Symbol.species)!;
  Object.defineProperty(Promise, Symbol.species, { get() { log.push("species"); return Promise; }, configurable: true });
  const r2 = returnsPromise(Promise.resolve("r2"));
  await flush();
  Object.defineProperty(Promise, Symbol.species, desc);
  r2.then((v) => log.push("r2:" + v));
  await flush();
  console.log(log.join(" "));
}
main();
