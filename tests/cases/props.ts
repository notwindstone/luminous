// @native counter readAll writeAll reorder bumpAll poly asyncOrder methodLoop
// Property access goes through inline caches keyed by shape: every way an
// object's shape or property can change must still be observed.

function counter(n: number): number {
  const hi: { value: number } = { value: 0 };
  for (let i = 0; i < n; i++) hi.value++;
  for (let i = 0; i < n; i++) ++hi.value;
  hi.value += 0.5;
  return hi.value;
}

function readAll(objs: { x: unknown }[]): string {
  const out: unknown[] = [];
  for (let i = 0; i < objs.length; i++) out.push(objs[i].x);
  return out.map(String).join(",");
}

function writeAll(objs: { x: unknown }[], v: unknown): string {
  const out: string[] = [];
  for (let i = 0; i < objs.length; i++) {
    try {
      objs[i].x = v;
      out.push(String(objs[i].x));
    } catch (e) {
      out.push((e as Error).name);
    }
  }
  return out.join(",");
}

// The object of an assignment is evaluated before its value.
function reorder(): string {
  let o: { x?: number; k?: number } = {};
  const first = o;
  o.x = ((o = { x: 0 }), 1);
  let p: { [k: string]: number } = {};
  const firstP = p;
  p[((p = {}), "k")] = 2;
  let q: { x: number } = { x: 10 };
  const firstQ = q;
  q.x += ((q = { x: 100 }), 1);
  let r: { x: number } = { x: 1 };
  r.x = r.x + 1;
  r.x++;
  const post = r.x++;
  const pre = ++r.x;
  return JSON.stringify([first, o, firstP, p, firstQ, q, r, post, pre]);
}

function bumpAll(objs: { n: unknown }[]): string {
  for (let i = 0; i < objs.length; i++) objs[i].n++;
  return objs.map((o) => String(o.n)).join(",");
}

// One site seeing many shapes.
function poly(objs: object[]): number {
  let s = 0;
  for (let r = 0; r < 3; r++)
    for (let i = 0; i < objs.length; i++) s += (objs[i] as { v: number }).v;
  return s;
}

async function asyncOrder(): Promise<string> {
  let o: { x?: number } = {};
  const first = o;
  o.x = (await Promise.resolve(1)) + ((o = {}), 0);
  let p: { x: number } = { x: 1 };
  p.x += await Promise.resolve(2);
  return JSON.stringify([first, o, p]);
}

class Point {
  constructor(public x: number, public y: number) {}
  move(dx: number): void {
    this.x += dx;
    this.y++;
  }
}

function methodLoop(n: number): string {
  const p = new Point(0, 0);
  for (let i = 0; i < n; i++) p.move(2);
  return `${p.x},${p.y}`;
}

console.log(counter(1000));

// Same shape, then shapes that change under the cache.
const a = { x: 1 };
const b = { x: 2 };
const objs: { x: unknown }[] = [a, b];
console.log(readAll(objs), readAll(objs));
const c = { x: 3 };
Object.defineProperty(c, "x", { get: () => "getter" });
const d = { x: 4 };
delete (d as { x?: number }).x;
const e = Object.create({ x: "inherited" });
const f = Object.freeze({ x: 6 });
const g = new Proxy({ x: 7 }, { get: () => "proxy" });
const h = { y: 0, x: 8 };
const arr = Object.assign([1, 2], { x: "array" });
const all = [a, b, c, d, e, f, g, h, arr] as { x: unknown }[];
console.log(readAll(all));
console.log(writeAll(all, 9));
console.log(readAll(all));
const sealed = Object.seal({ x: 1 });
const nonWritable = { x: 1 };
Object.defineProperty(nonWritable, "x", { writable: false });
console.log(writeAll([sealed, sealed, nonWritable, sealed], "w"));
// A cached object whose prototype changes.
const proto = { x: "proto" };
const swapped = { x: "own" };
readAll([swapped, swapped]);
Object.setPrototypeOf(swapped, proto);
console.log(readAll([swapped]));

console.log(reorder());
console.log(bumpAll([{ n: 1 }, { n: "5" }, { n: 2 ** 31 - 1 }, { n: -(2 ** 31) }, { n: 1.5 }, { n: null }]));
console.log(poly([{ v: 1 }, { a: 0, v: 2 }, { b: 0, c: 0, v: 3 }, Object.assign(Object.create({ v: 100 }), {})]));
asyncOrder().then(console.log);
console.log(methodLoop(1000));
