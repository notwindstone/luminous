// @native sum fill reads conversions bump first lengths elems plain nested
// Typed arrays are typed natively: parameters are guarded by class, and
// elements are numbers or `undefined` (out of bounds).

function sum(a: Float64Array): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i];
  return s;
}

function fill(a: Int32Array, v: number): Int32Array {
  for (let i = 0; i < a.length; i++) a[i] = v * i;
  return a;
}

// Out-of-bounds and non-integer indices read `undefined`.
function reads(a: Float64Array): string {
  const out: unknown[] = [];
  const x = a[100];
  out.push(x, typeof x, x === undefined, x == null, x ?? "dflt", "" + x, -x, +x, x | 0);
  out.push(a[0] === 1.5, a[0] !== 1.5, a[1.5], a[-1], a[NaN], a[-0]);
  out.push(Math.min(a[100]), a[0] + a[100], a[100] < 1, a[100] === a[100]);
  const y = a[0] > 1 ? a[0] : a[1];
  out.push(y, a[5] || "or", a[0] && a[1]);
  return out.map(String).join(",");
}

// Stores convert exactly like the engine.
function conversions(): string {
  const c = new Uint8ClampedArray(8);
  const vals = [0.5, 1.5, 2.5, -1, 300, NaN, 254.5, 1e20];
  for (let i = 0; i < 8; i++) c[i] = vals[i] as number;
  const i8 = new Int8Array(3);
  i8[0] = 200;
  i8[1] = -129.9;
  i8[2] = 2 ** 40 + 7;
  const u32 = new Uint32Array(2);
  u32[0] = -1;
  u32[1] = 2 ** 32 + 5.7;
  const f32 = new Float32Array(2);
  f32[0] = 0.1;
  f32[1] = 1e40;
  const u16 = new Uint16Array(1);
  u16[0] = 65536 + 3;
  const r = c[0] + c[1];
  return [Array.from(c), Array.from(i8), Array.from(u32), Array.from(f32), u16[0], r].join(" | ");
}

function bump(a: Float64Array): string {
  a[0]++;
  ++a[1];
  a[2] *= 3;
  a[3] -= 1;
  const old = a[4]--;
  a[50] = 7; // out of bounds: ignored
  a[1.5] = 7; // not an index: ignored
  return [Array.from(a), old, a[50]].join(" ");
}

function first(a: Float64Array): string {
  const v = a[0];
  return [v, typeof v, v === undefined, v !== v, String(v)].join(",");
}

function lengths(a: Float64Array, xs: number[], s: string): string {
  return [a.length, xs.length, s.length, "ünïcødé".length, ({ length: "own" }).length, sum.length].join(",");
}

// Plain arrays: elements are any JS values.
function elems(xs: unknown[]): string {
  const out: unknown[] = [];
  for (let i = 0; i < xs.length + 2; i++) out.push(xs[i]);
  xs[0] = "replaced";
  xs[xs.length] = "appended";
  xs[10] = "far";
  out.push(xs.length, xs[9], xs["1" as unknown as number]);
  return out.map(String).join(",");
}

function plain(xs: number[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += xs[i];
  for (let i = 1; i < xs.length; i++) xs[i] = xs[i] + xs[i - 1];
  return s + xs[xs.length - 1];
}

function nested(n: number): number {
  const grid = new Float64Array(n * n);
  for (let y = 0; y < n; y++) for (let x = 0; x < n; x++) grid[y * n + x] = x * y;
  const counts = new Uint8Array(256);
  for (let i = 0; i < grid.length; i++) counts[grid[i] & 255]++;
  let t = 0;
  for (let i = 0; i < 256; i++) t += counts[i] * i;
  return t;
}

const a = new Float64Array([1.5, 2, 3, 4, 5]);
console.log(sum(a), sum(new Float64Array(0)));
console.log(Array.from(fill(new Int32Array(4), 3)));
console.log(reads(a));
console.log(conversions());
console.log(bump(new Float64Array([1, 2, 3, 4, 5])));
console.log(lengths(a, [1, 2, 3], "hello"));
console.log(elems([1, "two", { three: 3 }]));
console.log(elems([]));
const holes: unknown[] = [1, , 3];
console.log(elems(holes));
console.log(plain([1, 2, 3, 4]), plain([]));
console.log(nested(40));

// Guards: other classes and non-arrays take the interpreted path.
console.log(sum(new Int32Array([1, 2, 3]) as unknown as Float64Array));
console.log(sum([1, 2, 3] as unknown as Float64Array));
console.log(sum({ length: 2, 0: 10, 1: 20 } as unknown as Float64Array));

// The reserved NaN, stored through another view, still reads as NaN.
const f = new Float64Array(1);
const words = new Uint32Array(f.buffer);
words[0] = 1;
words[1] = 0x7ff80000;
console.log(first(f), sum(f));

// An own `length` and a modified prototype are honored.
const own = new Float64Array([1, 2]);
Object.defineProperty(own, "length", { value: 1 });
console.log(sum(own), lengths(own, [], ""));
const reproto = new Float64Array([1, 2, 3]);
Object.setPrototypeOf(reproto, { __proto__: Float64Array.prototype, get length() { return 2; } });
console.log(sum(reproto));
try {
  Object.defineProperty(Float64Array.prototype, "length", { value: 0 });
} catch (e) {
  console.log("locked:", (e as Error).name);
}
try {
  (globalThis as any).Float64Array = Array;
} catch (e) {
  console.log("locked:", (e as Error).name);
}

// Detached and resizable buffers.
const buf = new ArrayBuffer(16, { maxByteLength: 64 });
const tracking = new Float64Array(buf);
tracking[1] = 9;
console.log(sum(tracking), tracking.length);
buf.resize(32);
tracking[3] = 1;
console.log(sum(tracking), tracking.length);
buf.resize(8);
console.log(sum(tracking), tracking.length, reads(tracking).split(",")[0]);
const moved = new Float64Array([1, 2, 3]);
moved.buffer.transfer();
moved[0] = 5;
console.log(sum(moved), moved.length, moved[0], reads(moved).split(",")[0]);

// Arrays whose elements are not plain data.
const getters: number[] = [1, 2, 3];
let stored = 0;
Object.defineProperty(getters, 1, { get: () => 20, set: (v: number) => void (stored = v) });
console.log(plain(getters), stored, getters);
const frozen = Object.freeze([1, 2, 3]) as number[];
try {
  plain(frozen);
} catch (e) {
  console.log("frozen:", (e as Error).name);
}
