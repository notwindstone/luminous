// @native keys nested reuse keyReassigned changeThenRead early thrown viaAwait viaYield protoEpoch sameShapeTwo
// for-in over an object enumerated by its shape: the shape's keys (shared,
// cached), stepped by native code with its own index; the same keys, in
// the same order, as any other way when the object or its prototypes change.
function keys(o: any): string {
  const r: string[] = [];
  for (const k in o) r.push(k);
  return r.join(",");
}

function nested(o: any): string {
  let out = "";
  for (const a in o) for (const b in o) out += a + b + " ";
  return out;
}

function reuse(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const o = { x: i, y: 2, z: 3 };
    for (const k in o) total += (o as any)[k];
  }
  return total;
}

function keyReassigned(o: any): string {
  let out = "";
  for (let k in o) {
    out += String(o[k]) + ":";
    k = "y";
    out += String(o[k]) + " ";
  }
  return out;
}

function changeThenRead(): string {
  const o: any = { a: 1, b: 2, c: 3, d: 4 };
  let out = "";
  for (const k in o) {
    if (k === "a") {
      delete o.b;
      o.e = 5;
      o.c = 30;
    }
    out += k + "=" + String(o[k]) + " ";
  }
  return out;
}

function early(o: any): string {
  let out = "";
  for (const k in o) {
    if (k === "b") continue;
    if (k === "d") break;
    out += k;
  }
  return out;
}

function thrown(o: any): string {
  let out = "";
  try {
    for (const k in o) {
      out += k;
      if (k === "b") throw new Error("at " + k);
    }
  } catch (e) {
    out += " " + (e as Error).message;
  }
  for (const k in o) out += k;
  return out;
}

async function viaAwait(o: any): Promise<string> {
  let out = "";
  for (const k in o) {
    await null;
    out += k + "=" + String(o[k]) + " ";
  }
  return out;
}

function* viaYield(o: any): Generator<string> {
  for (const k in o) yield k + String(o[k]);
}

function protoEpoch(): string {
  const proto: any = { };
  const o = Object.create(proto);
  o.own = 1;
  const out = [keys(o)];
  proto.inherited = 2;
  out.push(keys(o));
  Object.defineProperty(proto, "hidden", { value: 3, enumerable: false });
  out.push(keys(o));
  delete proto.inherited;
  out.push(keys(o));
  (Object.prototype as any).everywhere = 4;
  out.push(keys(o), keys({ p: 1 }));
  delete (Object.prototype as any).everywhere;
  out.push(keys(o), keys({ p: 1 }));
  return out.join(" | ");
}

function sameShapeTwo(a: any, b: any): string {
  let out = "";
  for (const k in a) out += String(b[k]) + String(a[k]) + " ";
  return out;
}

const plain = { a: 1, b: 2, c: 3, d: 4 };
console.log(keys(plain), keys(plain), keys({}), keys({ z: 1 }));
console.log(nested({ p: 1, q: 2 }));
console.log(reuse(100));
console.log(keyReassigned({ x: 1, y: 2 }));
console.log(changeThenRead());
console.log(early(plain), thrown(plain));
console.log(protoEpoch());
console.log(sameShapeTwo({ m: 1, n: 2 }, { m: 10, n: 20 }), sameShapeTwo({ m: 1, n: 2 }, { n: 20, m: 10 }));
console.log([...viaYield({ g: 1, h: 2 })].join(","));
console.log(keys([1, 2]), keys("ab"), keys(null), keys(undefined), keys(7), keys(new Proxy({ px: 1 }, {})));
const sym = Symbol("s");
const withSym: any = { s1: 1, [sym]: 2, s2: 3 };
Object.defineProperty(withSym, "ne", { value: 1, enumerable: false });
console.log(keys(withSym), keys({ 2: "b", 1: "a", k: "k" }));
const big: any = {};
for (let i = 0; i < 300; i++) big["p" + i] = i;
let bigSum = 0;
for (const k in big) bigSum += big[k];
console.log(bigSum, keys(big).length);
viaAwait({ u: 1, v: 2 }).then((s) => console.log(s));
