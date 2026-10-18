// @native loop primitives objects rejected locals fromThen deep caller callee pending unwinding ownCtor ctorGetter
// Awaits that continue at once (an async function resumed by its job,
// with no other job pending and a settled value) and awaits that must
// suspend: the order of everything the microtask queue runs, as the spec
// has it.
import { setTimeout } from "qjs:os";
const log: string[] = [];
const seven = Promise.resolve(7);

function ticker(name: string, n: number): void {
  let p: Promise<void> = Promise.resolve();
  for (let i = 0; i < n; i++) {
    const k = i;
    p = p.then(() => {
      log.push(name + " tick " + k);
    });
  }
}

async function loop(name: string, n: number): Promise<number> {
  let total = 0;
  for (let i = 0; i < n; i++) {
    total += await seven;
    log.push(name + " " + i);
  }
  return total;
}

async function primitives(): Promise<void> {
  const a = await 1;
  const b = await "s";
  const c = await null;
  const d = await undefined;
  const e = await true;
  const f = await 2.5;
  log.push("primitives " + a + " " + b + " " + c + " " + d + " " + e + " " + f);
}

class Box {
  v = 3;
  then = 4;
}

class Thenable {
  then(res: (v: number) => void): void {
    log.push("Thenable then");
    res(9);
  }
}

async function objects(): Promise<void> {
  const o = { a: 1 };
  const x = await o;
  log.push("plain " + (x === o));
  const arr = [1, 2];
  const y = await arr;
  log.push("array " + (y === arr));
  const box = new Box();
  const z = await box;
  log.push("then not a function " + (z === box));
  const t = await new Thenable();
  log.push("thenable " + t);
  const getter = {
    get then() {
      log.push("then getter");
      return undefined;
    },
  };
  const g = await getter;
  log.push("getter " + (g === getter));
  const n = await seven;
  log.push("after objects " + n);
}

async function rejected(): Promise<void> {
  await seven;
  try {
    await Promise.reject(new Error("no"));
  } catch (e) {
    log.push("caught " + (e as Error).message);
  }
  const v = await seven;
  log.push("after rejected " + v);
}

async function locals(): Promise<void> {
  let count = 0;
  let sum = 0.5;
  let flag = false;
  let text = "";
  let captured = 1;
  const bump = () => {
    captured *= 2;
  };
  for (let i = 0; i < 6; i++) {
    count += await 1;
    sum += await 0.25;
    flag = !(await flag);
    text += await "ab"[i % 2];
    bump();
    captured += await 1;
  }
  log.push("locals " + count + " " + sum + " " + flag + " " + text + " " + captured);
}

// Started by a reaction job (not resumed by its own): what follows the
// call in that job runs before its first await continues.
async function fromThen(): Promise<void> {
  log.push("fromThen start");
  await seven;
  log.push("fromThen 1");
  await seven;
  log.push("fromThen 2");
}

async function deep(n: number): Promise<number> {
  let s = 0;
  for (let i = 0; i < n; i++) s += await i;
  return s;
}

async function callee(k: number): Promise<number> {
  await seven;
  log.push("callee " + k);
  await seven;
  return k * 2;
}

async function caller(): Promise<void> {
  await seven;
  const a = await callee(1);
  log.push("caller got " + a);
  const b = await callee(2);
  log.push("caller got " + b);
}

async function pending(): Promise<void> {
  let resolve: (v: number) => void = () => {};
  const p = new Promise<number>((r) => {
    resolve = r;
  });
  Promise.resolve().then(() => {
    log.push("resolving");
    resolve(5);
  });
  const v = await p;
  log.push("pending " + v);
  const w = await p;
  log.push("pending again " + w);
}

async function unwinding(): Promise<void> {
  try {
    await seven;
    await seven;
    throw new Error("inside");
  } catch (e) {
    log.push("unwinding caught " + (e as Error).message);
  } finally {
    await seven;
    log.push("unwinding finally");
  }
  log.push("unwinding end");
}

// A promise with its own `constructor`: PromiseResolve reads it.
async function ownCtor(): Promise<void> {
  await seven;
  const p = Promise.resolve(11);
  (p as any).constructor = Promise;
  const v = await p;
  log.push("ownCtor " + v);
}

// `constructor` of Promise.prototype a getter: every await of a promise
// reads it.
async function ctorGetter(): Promise<void> {
  await seven;
  const v = await seven;
  log.push("ctorGetter " + v);
}

async function main(): Promise<void> {
  ticker("A", 4);
  const l = loop("loop", 6);
  ticker("B", 3);
  primitives();
  objects();
  rejected();
  locals();
  Promise.resolve().then(() => {
    fromThen();
    log.push("after fromThen call");
  });
  deep(20000).then((s) => log.push("deep " + s));
  caller();
  pending();
  unwinding();
  ownCtor();
  log.push("loop total " + (await l));
  setTimeout(() => {
    Object.defineProperty(Promise.prototype, "constructor", {
      get() {
        log.push("constructor read");
        return Promise;
      },
      configurable: true,
    });
    ticker("C", 2);
    ctorGetter();
    setTimeout(() => console.log(log.join("\n")), 0);
  }, 0);
}

main();
