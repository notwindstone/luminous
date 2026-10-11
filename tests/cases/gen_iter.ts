// @native counter sum early throwing delegating patched selfRef mixed
const log: string[] = [];

function* counter(n: number) {
  try {
    for (let i = 0; i < n; i++) yield i;
    return "done";
  } finally {
    log.push("finally " + n);
  }
}

function sum(n: number): number {
  let s = 0;
  for (const x of counter(n)) s += x;
  return s;
}

function early(): string {
  let out = "";
  for (const x of counter(10)) {
    if (x === 3) break;
    out += x;
  }
  const g = counter(5);
  for (const x of g) {
    if (x === 1) break;
  }
  out += JSON.stringify(g.next());
  try {
    for (const x of counter(4)) {
      if (x === 2) throw new Error("boom " + x);
    }
  } catch (e) {
    out += (e as Error).message;
  }
  return out;
}

function* throwing(): Generator<number> {
  yield 1;
  throw new Error("inner");
}

function* delegating(): Generator<unknown> {
  yield 0;
  yield* counter(2);
  yield* [10, 20];
  yield* { [Symbol.iterator]: () => ({ next: () => ({ done: false, value: "custom" }), return: () => { log.push("custom return"); return { done: true, value: undefined }; } }) };
}

function patched(): string {
  let out = "";
  const proto = Object.getPrototypeOf(counter(0)) as { next: unknown };
  const g = counter(3);
  for (const x of g) out += x;
  // An own `next` on a generator object: iteration must call it.
  const h = counter(3) as unknown as { next: () => IteratorResult<number> };
  let calls = 0;
  h.next = () => (calls++ < 2 ? { done: false, value: 7 } : { done: true, value: undefined });
  for (const x of h as unknown as Iterable<number>) out += x;
  out += "|" + calls + "|" + typeof proto.next;
  return out;
}

function* selfRef(): Generator<number> {
  for (const x of counter(2)) yield x * 100;
}

function mixed(): string {
  let out = "";
  try {
    for (const x of throwing()) out += x;
  } catch (e) {
    out += (e as Error).message;
  }
  let n = 0;
  for (const x of delegating()) {
    out += "," + String(x);
    if (++n === 7) break;
  }
  for (const x of selfRef()) out += ";" + x;
  const spent = counter(1);
  for (const x of spent) out += "a" + x;
  for (const x of spent) out += "b" + x;
  return out;
}

console.log(sum(0), sum(5), sum(100000));
console.log(early());
console.log(log.splice(0).join(" | "));
console.log(patched());
console.log(mixed());
console.log(log.splice(0).join(" | "));
// %GeneratorPrototype%.next patched globally: the direct path must not be used.
const GenProto = Object.getPrototypeOf(Object.getPrototypeOf(counter(0))) as { next: (this: Generator) => unknown };
const orig = GenProto.next;
let seen = 0;
GenProto.next = function (this: Generator) {
  seen++;
  return orig.call(this);
};
console.log(sum(4), seen);
GenProto.next = orig;
console.log(sum(4), seen, log.splice(0).join(" | "));
