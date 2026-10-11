// @native points unions nested arrays writes guarded describe sum scalar scalarChecks
// `aot type`: objects of a fixed layout, in native and interpreted code.

aot type Point = {
  x: int;
  y: int;
  z: int;
  kind: "colored";
  color: string;
} | {
  x: int;
  y: int;
  z: int;
  kind: "uncolored";
};

aot type AB = { a: number } | { b: string };

aot type XY = { x: int } & { y: number };

aot type Line = { from: XY; to: XY; label: string; visible: boolean };

function errorOf(f: () => void): string {
  try {
    f();
    return "ok";
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

function describe(p: Point): string {
  if (p.kind === "colored") return "colored " + p.color + " at " + (p.x + p.y + p.z);
  return "uncolored at " + (p.x + p.y + p.z);
}

function points(): string {
  const out: unknown[] = [];
  const a: Point = { x: 1, y: 2, z: 3, kind: "colored", color: "#fff" };
  const b: Point = { x: 4, y: 5, z: 6, kind: "uncolored" };
  out.push(describe(a), describe(b), JSON.stringify(a), JSON.stringify(b));
  out.push(Object.keys(a).join(","), Object.keys(b).join(","), "color" in a, "color" in b);
  out.push((b as { color?: string }).color, Object.isExtensible(a), Object.isSealed(b));
  // Another member, as long as the object still matches one.
  a.kind = "uncolored";
  out.push(describe(a), JSON.stringify(a));
  out.push(errorOf(() => ((b as { kind: string }).kind = "colored")), describe(b));
  out.push(errorOf(() => ((a as { kind: string }).kind = "other")));
  return out.join("\n");
}

function unions(): string {
  const out: unknown[] = [];
  const onlyA: AB = { a: 1.5 };
  const onlyB: AB = { b: "Eden Treaty" };
  const both: AB = { a: 10, b: "Decagrammaton" };
  for (const v of [onlyA, onlyB, both]) {
    out.push(JSON.stringify(v), "a" in v, "b" in v, (v as { a?: number }).a, (v as { b?: string }).b);
  }
  out.push(errorOf(() => ((onlyA as { b: string }).b = "late")));
  out.push(errorOf(() => delete (both as { a?: number }).a), JSON.stringify(both));
  return out.join(",");
}

function nested(): string {
  const l: Line = { from: { x: 1, y: 2.5 }, to: { x: 3, y: 4 }, label: "l", visible: true };
  l.from.x = l.from.x + l.to.x;
  l.to = { x: -1, y: -2 };
  const { from, to } = l;
  return [JSON.stringify(l), from.x * to.y, l.visible, typeof l.label].join(",");
}

function sum(n: int): number {
  let acc = 0;
  for (let i = 0; i < n; i++) {
    const p: XY = { x: i, y: i / 2 };
    acc += p.x * p.y;
  }
  return acc;
}

function arrays(): string {
  const pts: XY[] = new Array(3);
  for (let i = 0; i < 3; i++) pts[i] = { x: i, y: i + 0.5 };
  pts.push({ x: 9, y: 9 });
  const more: Array<XY> = [{ x: 7, y: 7 }];
  let s = 0;
  for (let i = 0; i < pts.length; i++) {
    const { x, y } = pts[i];
    s += x + y;
  }
  return [s, JSON.stringify(pts), JSON.stringify(more)].join("|");
}

function writes(): string {
  const p: XY = { x: 1, y: 1 };
  const out: unknown[] = [];
  p.x = 2 ** 31 + 5;
  out.push(p.x);
  p.x++;
  out.push(p.x);
  p.x = -0;
  out.push(Object.is(p.x, 0));
  out.push(errorOf(() => (p.x = 1.25)), errorOf(() => ((p as { x: unknown }).x = "1")), p.x);
  out.push(errorOf(() => ((p as { y: unknown }).y = null)), p.y);
  out.push(errorOf(() => ((p as Record<string, unknown>).z = 1)));
  out.push(errorOf(() => Object.defineProperty(p, "x", { get: () => 1 })));
  out.push(errorOf(() => Object.freeze(p)));
  out.push(errorOf(() => Object.defineProperty(p, "y", { value: 8 })), p.y);
  out.push(JSON.stringify({ ...p }), Object.entries(p).join(";"));
  return out.join(",");
}

function guarded(): string {
  // A plain object where an `aot type` is declared: the native entry
  // declines and the interpreted fallback runs.
  const plain = JSON.parse('{"x":1,"y":2,"z":3,"kind":"uncolored"}');
  return describe(plain) + "|" + describe({ x: 0, y: 0, z: 0, kind: "uncolored" });
}

interpreted function interpretedMake(): XY {
  return { x: 5, y: 6 };
}

console.log(points());
console.log(unions());
console.log(nested());
console.log(sum(100));
console.log(arrays());
console.log(writes());
console.log(guarded());
const im = interpretedMake();
console.log(JSON.stringify(im), Object.isExtensible(im), errorOf(() => ((im as Record<string, unknown>).q = 1)));

aot type Named = { id: int; name: string; ratio: number; on: boolean };

// Objects that do not escape become locals: stores still convert or throw.
function scalar(): string {
  const p: XY = { x: 1, y: 2 };
  p.x = p.x + 2 ** 31;
  const wrapped = p.x;
  let msg = "";
  try {
    p.x = p.y / 4;
  } catch (e) {
    msg = (e as Error).message;
  }
  const n: Named = { id: 3, name: "n", ratio: 0.5, on: true };
  n.id = n.id * 1000000000;
  n.ratio = n.ratio + n.id;
  n.on = !n.on;
  n.name = n.name + n.id;
  return [wrapped, p.x, p.y, msg, n.id, n.ratio, n.on, n.name].join(",");
}
console.log(scalar());

function scalarChecks(): string {
  const n: Named = { id: 1, name: "a", ratio: 1, on: false };
  let msg = "";
  try {
    n.name = (n.id as unknown) as string;
  } catch (e) {
    msg = (e as Error).message;
  }
  return n.name + "," + msg;
}
console.log(scalarChecks());
