// @native len make sumLens returned annotated rare nullProto identity nested setter order throws loopMade
// Objects through inlined calls: an object passed to a small function
// that is inlined is read there through its fields, and one an inlined
// function returns is the caller's own: neither is allocated. Where one
// is needed as an object (a rare branch, identity), it is made then.
type P = { x: number; y: number };

function len(p: any): number {
  return p.x + p.y;
}

function make(a: number, b: number): any {
  return { x: a, y: b };
}

function sumLens(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const p = { x: i, y: i * 2 };
    s += len(p);
  }
  return s;
}

function returned(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const q = make(i, 3);
    s += q.x * q.y;
  }
  return s;
}

function dot(a: P, b: P): number {
  return a.x * b.x + a.y * b.y;
}

function annotated(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const a: P = { x: i, y: 1 };
    const b: P = { x: 2, y: i };
    s += dot(a, b);
  }
  return s;
}

const seen: any[] = [];
function rare(n: number, every: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const p = { x: i, y: -i };
    if (i % every === 0) seen.push(p);
    p.x += 1;
    s += len(p);
  }
  return s;
}

function nullProto(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const o: any = Object.create(null);
    o.x = i;
    o.y = 1;
    s += len(o) + (o.z === undefined ? 1 : 0);
  }
  return s;
}

function same(a: any, b: any): boolean {
  return a === b;
}

function identity(): string {
  const p = { x: 1, y: 2 };
  const q = { x: 1, y: 2 };
  return `${same(p, p)} ${same(p, q)} ${len(p)}`;
}

function inner(p: any): number {
  return len(p) * 2;
}

function nested(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += inner({ x: i, y: 1 });
  return s;
}

let setterCalls = 0;
function setter(): number {
  // A setter on Object.prototype for a key added later: the object is
  // made before the addition, which runs the setter.
  Object.defineProperty(Object.prototype, "z", {
    set(v) {
      setterCalls += v;
    },
    configurable: true,
  });
  const o: any = make(1, 2);
  o.z = 5;
  const r = len(o) + setterCalls + (Object.keys(o).length * 100);
  delete (Object.prototype as any).z;
  return r;
}

function order(): string {
  const o = make(1, 2);
  (o as any).w = 3;
  return Object.keys(o).join(",") + " " + JSON.stringify(o);
}

function throws(p: any): number {
  if (p.x > 2) throw new Error("big " + p.x);
  return p.x;
}

function loopMade(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    try {
      out += throws({ x: i, y: 0 });
    } catch (e: any) {
      out += "[" + e.message + "]";
    }
  }
  return out;
}

console.log(sumLens(1000), returned(1000), annotated(1000));
console.log(rare(100, 33), seen.length, seen.map((p) => p.x + ":" + p.y).join(" "));
console.log(nullProto(100), identity(), nested(100));
console.log(setter(), setterCalls, order(), loopMade(5));
