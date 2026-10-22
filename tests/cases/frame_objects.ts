// @native fill bump total serialize destructure enumerate depth layer pinned grabbed counted inherited grow points stepped across later boom caught thrower
// @env QJS_GC_STRESS=5
// Frame objects: objects that must be objects (written by the functions
// they are passed to, serialized, enumerated) but never outlive the call
// that makes them, made in a slot of its frame. They are real objects:
// identity, key order, JSON. Where code that may keep one runs (a toJSON,
// a replaced builtin, an inherited getter read by for-in), the frame
// region is pinned and the object stays; the region running out sends
// objects to the heap; an async function's objects that live across
// awaits are in its frame's cell.
import { setTimeout } from "qjs:os";

type Rec = { a: number; b: string; list: number[]; tag: any };

function fill(r: Rec, n: number): void {
  for (let i = 0; i < 2; i++) {
    r.a += n + i;
    r.list = r.list.concat([n * 10 + i]);
  }
  r.tag = { n: n, s: "t" + n };
  r.b = r.b + "|" + n;
}

function bump(r: Rec): void {
  for (let i = 0; i < 3; i++) r.a += i;
}

function total(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const r = { a: i, b: "r" + i, list: [] as number[], tag: null as any };
    fill(r, i);
    bump(r);
    // Garbage while the frame object holds young values.
    let junk = 0;
    for (let k = 0; k < 20; k++) junk += [k, k + 1, "x" + k].length;
    out += `${r.a}:${r.b}:${r.list.join("/")}:${r.tag.s}:${junk};`;
  }
  return out;
}

function serialize(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const o = { id: i, name: "n" + i, ok: i % 2 === 0, nested: [i, { d: i }] };
    out += JSON.stringify(o);
    out += Object.keys(o).join(",") + Object.keys(o).length;
    out += JSON.stringify(Object.values(o)) + JSON.stringify(Object.entries(o));
    const copy = { ...o };
    out += copy.name + (copy === o ? "=" : "!");
  }
  return out;
}

// Copies of the local: objects in the heap.
function destructure(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const o = { id: i, name: "n" + i, rest: [i] };
    const { id, ...rest } = o;
    const same = o;
    out += id + JSON.stringify(rest) + (same === o ? "=" : "!");
  }
  return out;
}

function enumerate(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const o = { x: i, y: i * 2, z: "z" + i };
    for (const k in o) out += k + "=" + (o as any)[k] + ";";
  }
  return out;
}

// Deep recursion: past the frame region's room, objects go to the heap.
function depth(d: number): number {
  const a = { d: d, s: "s" + d, v: 0, p0: 0, p1: 1, p2: 2, p3: 3, p4: 4, p5: 5, p6: 6, p7: 7, p8: 8, p9: 9 };
  const b = { d: d + 1, s: "u", v: 1, p0: 0, p1: 1, p2: 2, p3: 3, p4: 4, p5: 5, p6: 6, p7: 7, p8: 8, p9: 9 };
  layer(a, b);
  if (d > 0) a.v += depth(d - 1);
  return a.v + b.v + a.s.length + b.p9;
}

function layer(a: any, b: any): void {
  for (let i = 0; i < 2; i++) {
    a.v += b.d;
    b.v += a.d;
  }
}

// Code that may keep the objects it is given.
const kept: any[] = [];

function pinned(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const o = { id: i, label: "L" + i };
    out += JSON.stringify(o);
  }
  return out;
}

function grabbed(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const o = { k: i, w: "w" + i };
    s += Object.keys(o).length;
  }
  return s;
}

function counted(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const o = { k: i, v: i + 1 };
    s += Object.keys(o).length + o.v;
  }
  return s;
}

function inherited(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const o = { p: i, q: "q" + i };
    for (const k in o) out += k + ":" + (o as any)[k] + ",";
  }
  return out;
}

// Class instances whose constructor is inlined, given to methods.
class Point {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  scale(k: number): void {
    for (let i = 0; i < 2; i++) {
      this.x *= k;
      this.y *= k;
    }
  }
}

function grow(p: Point, k: number): void {
  for (let i = 0; i < 2; i++) p.x += k;
}

function points(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const p = new Point(i, i + 1);
    grow(p, 3);
    out += JSON.stringify(p) + Object.keys(p).length + p.y + ";";
    const q = new Point(i, -i);
    q.scale(2);
    out += JSON.stringify(q) + ";";
  }
  return out;
}

// Exceptions: the object stays usable after the callee throws.
function boom(r: Rec, n: number): void {
  r.a += n;
  if (n % 3 === 0) throw new Error("boom " + n);
  r.b = "ok";
}

function caught(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const r = { a: i, b: "-", list: [i], tag: 0 };
    try {
      boom(r, i);
    } catch (e: any) {
      out += "[" + e.message + "]";
    }
    out += r.a + r.b + ";";
  }
  return out;
}

function thrower(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const o = { code: i, msg: "m" + i };
    try {
      if (i % 2 === 1) throw o;
      out += o.code;
    } catch (e: any) {
      out += "<" + e.msg + ">";
      kept.push(e);
    }
  }
  return out;
}

// Async: objects across awaits (in the frame's cell), objects made and
// serialized between awaits (in the frame region, for a step).
async function stepped(r: Rec, k: number): Promise<void> {
  await null;
  for (let i = 0; i < 2; i++) r.a += k;
  await null;
  r.b = r.b + "~" + k;
}

async function across(n: number): Promise<string> {
  let out = "";
  for (let i = 0; i < n; i++) {
    const r = { a: i, b: "c" + i, list: [i], tag: null as any };
    await stepped(r, 2);
    r.tag = { t: i };
    await null;
    await stepped(r, 3);
    out += `${r.a}${r.b}${r.tag.t};`;
    await null;
    const log = { at: i, what: "done", n: r.a };
    out += JSON.stringify(log) + Object.keys(log).length;
  }
  return out;
}

// An async callee not awaited keeps its argument: a heap object.
async function later(r: Rec): Promise<void> {
  await null;
  r.a += 100;
  kept.push(r);
}

function launch(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const r = { a: i, b: "", list: [], tag: 0 };
    later(r);
    r.a += 1;
    s += r.a;
  }
  return s;
}

function timers(n: number, done: (s: string) => void): void {
  const o = { n: n, parts: ["t"] };
  setTimeout(() => {
    o.parts.push("fired" + o.n);
    done(o.parts.join("-"));
  }, 0);
  o.n += 1;
}

async function main(): Promise<void> {
  console.log(total(40));
  console.log(serialize(10));
  console.log(destructure(4));
  console.log(enumerate(6));
  console.log(depth(1200));
  console.log(depth(10));

  console.log(pinned(3));
  (Object.prototype as any).toJSON = function (this: any) {
    kept.push(this);
    return "T" + this.id;
  };
  console.log(pinned(3));
  delete (Object.prototype as any).toJSON;
  console.log(total(5).length, depth(50));

  const keys = Object.keys;
  (Object as any).keys = function (o: any) {
    kept.push(o);
    return keys(o);
  };
  console.log(grabbed(3), counted(3));
  (Object as any).keys = keys;
  console.log(grabbed(3), counted(3));

  Object.defineProperty(Object.prototype, "extra", {
    get: function (this: any) {
      kept.push(this);
      return "E";
    },
    enumerable: true,
    configurable: true,
  });
  console.log(inherited(3));
  delete (Object.prototype as any).extra;
  console.log(inherited(2), total(3).length);

  console.log(points(4));
  console.log(caught(7));
  console.log(thrower(5));
  console.log(await across(6));
  console.log(launch(4));
  await null;
  await null;
  await null;
  console.log(
    kept.length,
    kept.map((o: any) => (typeof o === "object" ? JSON.stringify(o) : String(o))).join(" "),
  );
  timers(7, (s) => console.log(s));
}

main();
