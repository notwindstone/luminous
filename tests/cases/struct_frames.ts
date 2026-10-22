// @native addAll sum step run kept keepIt later notAwaited deep stringified checked optional shaped
// Objects of `aot type`s made by literals are made inline, and where they
// never outlive the call, in its frame (see src/frames.rs): their fields
// (strings included, which the collector traces there) read and written
// by helpers that are not inlined, across awaits, in loops that reuse the
// slot; while one kept by a global, given to an async call not awaited,
// serialized, or made past the frame region's room stays an ordinary
// object; and fields that values do not fit still throw.
aot type Acc = { total: number; count: int; label: string };
aot type Opt = { id: int; note?: string };
aot type Shape = { kind: "circle"; r: number } | { kind: "rect"; w: number; h: number };

function addAll(a: Acc, n: number): void {
  for (let i = 0; i < n; i++) {
    a.total += i * 0.5;
    a.count += 1;
    a.label = a.label + "+";
  }
}
function sum(n: number): number {
  let s = 0;
  for (let r = 0; r < n; r++) {
    const acc: Acc = { total: r, count: 0, label: "r" + (r % 3) };
    addAll(acc, 3);
    s += acc.total + acc.count + acc.label.length;
  }
  return s;
}

async function step(a: Acc, k: number): Promise<number> {
  await Promise.resolve();
  a.total += k;
  a.label = a.label + k;
  return a.count;
}
async function run(n: number): Promise<string> {
  let s = 0;
  let last = "";
  for (let r = 0; r < n; r++) {
    const acc: Acc = { total: r, count: 1, label: "a" + r };
    s += await step(acc, 2);
    s += await step(acc, 3);
    s += acc.total;
    last = acc.label;
  }
  return s + " " + last;
}

// Kept by a global: an ordinary object.
let held: Acc | null = null;
function keepIt(a: Acc): void {
  held = a;
}
function kept(): number {
  const acc: Acc = { total: 1, count: 2, label: "kept" };
  keepIt(acc);
  acc.total = 5;
  return acc.total;
}

// Given to an async call not awaited: it reads the object later.
async function later(a: Acc): Promise<string> {
  await Promise.resolve();
  await Promise.resolve();
  return a.label + a.total;
}
async function notAwaited(): Promise<string> {
  const acc: Acc = { total: 7, count: 0, label: "late" };
  const p = later(acc);
  acc.total = 8;
  await Promise.resolve();
  return p;
}

// Past the frame region's room (six objects a frame, alive across the
// call): the deep frames' objects in the heap.
function deep(n: number): number {
  const a: Acc = { total: n, count: 1, label: "a" };
  const b: Acc = { total: 1, count: 1, label: "b" };
  const c: Acc = { total: 2, count: 1, label: "c" };
  const d: Acc = { total: 3, count: 1, label: "d" };
  const e: Acc = { total: 4, count: 1, label: "e" };
  const f: Acc = { total: 5, count: 1, label: "f" };
  addAll(a, 1);
  addAll(b, 1);
  addAll(c, 1);
  addAll(d, 1);
  addAll(e, 1);
  addAll(f, 1);
  const below = n === 0 ? 0 : deep(n - 1);
  return a.total + b.total + c.total + d.total + e.total + f.total + a.label.length + f.label.length + below;
}

function stringified(r: number): string {
  const acc: Acc = { total: r, count: 2, label: "json" };
  addAll(acc, 2);
  return JSON.stringify(acc);
}

// A value that does not fit a field: a TypeError, from the literal.
function checked(x: any): string {
  try {
    const acc: Acc = { total: x, count: 1, label: "c" };
    addAll(acc, 1);
    return "made " + acc.total;
  } catch (e) {
    return (e as Error).constructor.name;
  }
}
function optional(note: any): string {
  const o: Opt = { id: 1, note };
  return Object.keys(o).join(",") + " " + String(o.note);
}
function shaped(round: boolean): number {
  const s: Shape = round ? { kind: "circle", r: 2 } : { kind: "rect", w: 2, h: 3 };
  return s.kind === "circle" ? s.r * s.r : s.w * s.h;
}

console.log("sum", sum(20000));
console.log("kept", kept(), held && held.total, held && held.label);
console.log("deep", deep(600));
console.log("stringified", stringified(3));
console.log("checked", checked(1.5), checked("x"), checked(undefined));
console.log("optional", optional("n"), optional(undefined));
console.log("shaped", shaped(true), shaped(false));
run(2000).then((v) => console.log("run", v));
notAwaited().then((v) => console.log("notAwaited", v));
