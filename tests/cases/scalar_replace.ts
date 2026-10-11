// @native counter arith strings retypeMixed order dupKeys perIteration escapes missing zeros postfix gen asyncFields reassign swap otherKeys asValue inUpdate destructured
function counter(): number {
  const hi: { value: number } = { value: 0 };
  for (let i = 0; i < 1000; i++) {
    for (let j = 0; j < 100; j++) hi.value++;
  }
  return hi.value;
}

function arith(): string {
  const o = { a: 10, b: 0.5, c: 3 };
  o.a += 5;
  o.a -= 0.25;
  o.b *= 4;
  o.c /= 2;
  o.a--;
  o.c = o.a + o.b;
  return [o.a, o.b, o.c].join(",");
}

function strings(): string {
  const o = { s: "a", n: 1 };
  for (let i = 0; i < 3; i++) {
    o.s += "b";
    o.n += o.n;
  }
  return o.s + o.n;
}

function retypeMixed(flag: boolean): string {
  const o: { v: number | string } = { v: 1 };
  o.v = (o.v as number) + 1;
  if (flag) o.v = "str";
  return typeof o.v + ":" + o.v;
}

function order(): string {
  const log: string[] = [];
  const f = (x: string): number => {
    log.push(x);
    return log.length;
  };
  const o = { b: f("b"), a: f("a"), c: f("c") };
  o.a = f("set a") + o.c;
  return log.join(",") + "|" + o.a + "," + o.b + "," + o.c;
}

function dupKeys(): string {
  // eslint-disable-next-line no-dupe-keys
  const o = { a: 1, b: 2, ...{} } as { a: number; b: number };
  const p = { x: 1, y: 2 };
  p.x = p.y;
  p.y = 10;
  return o.a + "," + o.b + "," + p.x + "," + p.y;
}

function perIteration(): string {
  let out = "";
  for (let i = 0; i < 3; i++) {
    const o = { n: i, t: 0 };
    o.t += o.n * 10;
    o.n++;
    out += o.n + ":" + o.t + ";";
  }
  return out;
}

function show(o: { v: number }): number {
  return o.v;
}

function escapes(): string {
  const a = { v: 1 };
  a.v++;
  const r1 = show(a);
  const b = { v: 2 };
  const same = b === b;
  const c = { v: 3 };
  const json = JSON.stringify(c);
  const d = { v: 4 };
  const has = "v" in d;
  const e = { v: 5 };
  const key = "v";
  const viaKey = (e as Record<string, number>)[key];
  const f = { v: 6 };
  let keys = "";
  for (const k in f) keys += k;
  const g = { v: 7 };
  const read = () => g.v;
  g.v = 70;
  return [r1, same, json, has, viaKey, keys, read()].join(",");
}

function missing(): string {
  const o = { v: 1 } as { v: number; w?: number };
  const w = o.w;
  const t = typeof (o as unknown as { toString: unknown }).toString;
  const p = { v: 1 } as { v: number; w?: number };
  p.w = 5;
  return String(w) + "," + t + "," + p.w + "," + Object.keys(p).join("");
}

function zeros(): string {
  const o = { a: -0, b: 0, c: NaN, d: Infinity };
  o.b -= 0;
  o.d -= 1;
  return [Object.is(o.a, -0), Object.is(o.b, -0), o.c, o.d].join(",");
}

function postfix(): string {
  const o = { v: 5 };
  const old = o.v++;
  const old2 = o.v--;
  const pre = ++o.v;
  return [old, old2, pre, o.v].join(",");
}

function* gen(): Generator<number> {
  const o = { n: 0 };
  while (o.n < 3) {
    yield o.n;
    o.n++;
  }
}

async function asyncFields(): Promise<string> {
  const o = { n: 0, s: "" };
  for (let i = 0; i < 3; i++) {
    o.s += await Promise.resolve("x");
    o.n += 1;
  }
  return o.s + o.n;
}

function reassign(): string {
  let hi = { value: 0, tag: "a" };
  for (let i = 0; i < 100; i++) {
    if (i === 50) hi = { tag: "b", value: 1000 };
    hi.value++;
  }
  return hi.value + hi.tag;
}

function swap(): string {
  let p = { a: 1, b: 2 };
  p = { a: p.b, b: p.a };
  return p.a + "," + p.b;
}

function otherKeys(): string {
  let p: { a: number; b?: number } = { a: 1 };
  p.a++;
  p = { a: 5, b: 6 };
  return p.a + "," + p.b;
}

function asValue(): string {
  let p = { a: 1 };
  const q = (p = { a: 2 });
  q.a = 7;
  return p.a + "," + q.a;
}

function inUpdate(): string {
  let out = "";
  for (let p = { a: 0 }; p.a < 3; p = { a: p.a + 1 }) out += p.a;
  return out;
}

function toJson(o: object): string {
  return JSON.stringify(o);
}

function destructured(): string {
  let acc = 0;
  for (let i = 0; i < 5; i++) {
    const p = { x: i, y: i + 1, z: i * 2 };
    const { x, y, z } = p;
    acc += x + y + z;
  }
  const q = { a: 1, b: "two" };
  const { a, b: renamed, c = "default" } = q as { a: number; b: string; c?: string };
  const r = { m: 1, n: 2 };
  const { m, ...rest } = r;
  const s = { u: 3 };
  const { u } = s;
  const t = { v: 4 };
  const { v } = t;
  const seen = toJson(t);
  let w = { k: 1 };
  const { k } = w;
  w = { k: 2 };
  return [acc, a, renamed, c, m, toJson(rest), u, v, seen, k, w.k].join(",");
}

console.log(counter());
console.log(destructured());
console.log(arith());
console.log(strings());
console.log(retypeMixed(false), retypeMixed(true));
console.log(order());
console.log(dupKeys());
console.log(perIteration());
console.log(escapes());
console.log(missing());
console.log(zeros());
console.log(postfix());
console.log([...gen()].join(","));
console.log(reassign(), swap(), otherKeys(), asValue(), inUpdate());
asyncFields().then((s) => console.log("async", s));
