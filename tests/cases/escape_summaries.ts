// @native readXY sumObjects storeSometimes keepSometimes level1 level2 level3 chain viaCallback callbacks walk recursion even odd mutual same identity thrower catches finallyReads tryFinally ptLen points v2len structs useGen
// Escape summaries: a function that only reads the object a parameter
// receives is called, by a caller whose object does not escape otherwise,
// through a clone that takes the fields it reads; no object is made. Any
// other use (stored, captured, compared, thrown, passed to an unknown
// function or a generator) keeps the object, whose identity and
// mutations stay those of JS.
const external = { n: 5 };
let stored: any = null;

function readXY(p: any): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += p.x * i + p.y;
  return s;
}

function sumObjects(n: number): number {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const o = { x: i, y: 1 };
    t += readXY(o);
  }
  return t;
}

// Stores its object in a global on one path only: it escapes.
function storeSometimes(p: any, keep: boolean): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += p.x;
  if (keep) stored = p;
  return s;
}

function keepSometimes(n: number): string {
  let t = 0;
  let last: any = null;
  for (let i = 0; i < n; i++) {
    const o = { x: i, y: 0 };
    t += storeSometimes(o, i === 3);
    if (i === 3) last = o;
  }
  stored.x += 100;
  return `${t} ${stored === last} ${last.x}`;
}

// Passed on through three levels.
function level3(p: any): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += p.a;
  return s + p.b;
}

function level2(p: any, k: number): number {
  let s = 0;
  for (let i = 0; i < k; i++) s += level3(p);
  return s;
}

function level1(p: any): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += level2(p, i + 1) + p.c;
  return s;
}

function chain(n: number): number {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const o = { a: i, b: 2, c: 3 };
    t += level1(o);
  }
  return t;
}

// Passed to a callback it does not know: it escapes.
function viaCallback(p: any, cb: (x: any) => number): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += p.a;
  return s + cb(p);
}

function callbacks(n: number): string {
  const seen: any[] = [];
  let t = 0;
  for (let i = 0; i < n; i++) {
    const o = { a: i, b: 1 };
    t += viaCallback(o, (x) => {
      seen.push(x);
      return x.b;
    });
    o.b = 7;
  }
  return `${t} ${seen.length} ${seen[0] === seen[1]} ${seen[0].b}`;
}

// Recursion, and mutual recursion.
function walk(p: any, n: number): number {
  let s = 0;
  for (let i = 0; i < 1; i++) s += p.y;
  if (n === 0) return p.x + s;
  return walk(p, n - 1) + s;
}

function recursion(n: number): number {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const o = { x: i, y: 2 };
    t += walk(o, 3);
  }
  return t;
}

function even(p: any, n: number): number {
  let s = 0;
  for (let i = 0; i < 1; i++) s += p.e;
  return n === 0 ? s : s + odd(p, n - 1);
}

function odd(p: any, n: number): number {
  let s = 0;
  for (let i = 0; i < 1; i++) s += p.o;
  return n === 0 ? s : s + even(p, n - 1);
}

function mutual(n: number): number {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const o = { e: 1, o: i };
    t += even(o, 5);
  }
  return t;
}

// Compared by identity: it escapes.
function same(a: any, b: any): boolean {
  let r = false;
  for (let i = 0; i < 1; i++) r = a === b;
  return r;
}

function identity(): string {
  const o = { x: 1 };
  const q = { x: 1 };
  return `${same(o, o)} ${same(o, q)} ${same(q, q)}`;
}

// Thrown: it escapes.
function thrower(p: any): number {
  for (let i = 0; i < 1; i++) if (p.x > 1) throw p;
  return p.x;
}

function catches(): string {
  let out = "";
  for (let i = 0; i < 4; i++) {
    const o = { x: i };
    try {
      out += thrower(o);
    } catch (e) {
      out += "[" + (e === o) + (e as any).x + "]";
    }
  }
  return out;
}

// Read in a `try` and its `finally`.
function finallyReads(p: any): number {
  let s = 0;
  try {
    for (let i = 0; i < 2; i++) s += p.x;
    if (p.y) throw new Error("y");
  } finally {
    s += p.x * 10;
  }
  return s;
}

function tryFinally(): string {
  let out = "";
  for (let i = 0; i < 3; i++) {
    const o = { x: i, y: i === 1 };
    try {
      out += finallyReads(o) + ",";
    } catch (e) {
      out += (e as Error).message + ",";
    }
  }
  return out;
}

// Class instances.
class Pt {
  constructor(
    public x: number,
    public y: number,
  ) {}
}

function ptLen(p: Pt): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += p.x * p.x + p.y * p.y;
  return Math.sqrt(s);
}

function points(n: number): string {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const p = new Pt(i, 1);
    t += ptLen(p);
  }
  return t.toFixed(4);
}

// `aot type` objects.
aot type V2 = { x: number; y: number; k: int };

function v2len(v: V2): number {
  let s = 0;
  for (let i = 0; i < v.k; i++) s += v.x * v.y;
  return s;
}

function structs(n: number): number {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const v: V2 = { x: i, y: 2, k: 3 };
    t += v2len(v);
  }
  return t;
}

// A generator keeps its arguments: it escapes, and sees later writes.
function* gen(p: any): Generator<number> {
  for (let i = 0; i < 2; i++) yield p.x + i;
}

function useGen(): string {
  const o = { x: 1 };
  const g = gen(o);
  o.x = 10;
  return [...g].join(",");
}

console.log(sumObjects(100), keepSometimes(6), chain(10), callbacks(3));
console.log(recursion(10), mutual(10), identity(), catches(), tryFinally());
console.log(points(10), structs(10), useGen(), external.n);
