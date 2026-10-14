// @native mk advance pick last sums partial nearest clamped
// @interpreted replaceDot restoreDot
// Numeric regions over loops that read objects from arrays: the loop's
// invariant objects are checked once, each element in its iteration (its
// shape, its number fields, the objects its fields hold, not being one of
// the region's other objects). An element that is not what the region
// needs (another shape, a string field, a hole, the same object twice) has
// the original loop go on from that iteration, with the same results; so
// do arrays with holes past their elements, fractional indices, ints in
// the fields, `break` and `continue`, and a replaced method.

type B = { x: number; y: number; vx: number; vy: number; m: number };

function mk(n: number, ints: boolean): B[] {
  const out: B[] = [];
  for (let i = 0; i < n; i++) {
    const f = ints ? 1 : 1.25;
    out.push({ x: i * f, y: (i % 3) * f - 1, vx: 0, vy: ints ? 1 : 0.5, m: 1 + (i % 2) * f });
  }
  return out;
}

function advance(bs: B[], dt: number): string {
  const n = bs.length;
  for (let i = 0; i < n; i++) {
    const bi = bs[i];
    for (let j = i + 1; j < n; j++) {
      const bj = bs[j];
      const dx = bi.x - bj.x;
      const dy = bi.y - bj.y;
      const d2 = dx * dx + dy * dy + 0.01;
      const mag = dt / (d2 * Math.sqrt(d2));
      bi.vx -= dx * bj.m * mag;
      bi.vy -= dy * bj.m * mag;
      bj.vx += dx * bi.m * mag;
      bj.vy += dy * bi.m * mag;
    }
  }
  for (let i = 0; i < n; i++) {
    const b = bs[i];
    b.x += dt * b.vx;
    b.y += dt * b.vy;
  }
  let s = 0;
  for (let i = 0; i < n; i++) s += bs[i].x * 3 + bs[i].vy;
  return s.toFixed(9);
}

// The largest `x` and where it is; `continue` past negative ones, `break`
// at a sentinel.
function pick(bs: B[]): string {
  let best = -Infinity;
  let at = -1;
  for (let i = 0; i < bs.length; i++) {
    const b = bs[i];
    if (b.x < 0) continue;
    if (b.m === 99) break;
    if (b.x > best) {
      best = b.x;
      at = i;
    }
  }
  return best + "@" + at;
}

// A loop-carried value assigned only sometimes, read after the loop.
function last(bs: B[], k: number): string {
  let found: unknown = "none";
  let count = 0;
  for (let i = 0; i < bs.length; i += k) {
    const b = bs[i];
    if (b.y > 0) {
      found = b.x + b.y;
      count++;
    }
  }
  return String(found) + "/" + count;
}

function sums(bs: B[]): number {
  let s = 0;
  for (let i = 0; i < bs.length; i++) s += bs[i].x - bs[i].vy;
  return s;
}

// Reads from two arrays, and writes into the second.
function partial(a: B[], b: B[]): string {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    b[i].vx = a[i].x * 2 + b[i].vx;
    b[i].m = 3;
  }
  return b.map((p) => p.vx + ":" + p.m).join(",");
}

class V {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  dot(o: V): number {
    return this.x * o.x + this.y * o.y;
  }
  clampX(lo: number): number {
    if (this.x < lo) return lo;
    return this.x;
  }
}

class Site {
  at: V;
  w: number;
  constructor(at: V, w: number) {
    this.at = at;
    this.w = w;
  }
}

// Methods run inline on elements and on the objects their fields hold.
function nearest(ss: Site[], d: V): string {
  let best = -1;
  let score = -Infinity;
  for (let i = 0; i < ss.length; i++) {
    const s = ss[i].at.dot(d) * ss[i].w;
    if (s > score) {
      score = s;
      best = i;
    }
  }
  return best + ":" + score;
}

// A method with an early `return`.
function clamped(vs: V[], lo: number): number {
  let t = 0;
  for (let i = 0; i < vs.length; i++) t += vs[i].clampX(lo);
  return t;
}

let savedDot: unknown;
function replaceDot(): void {
  "use no aot";
  savedDot = V.prototype.dot;
  V.prototype.dot = function (this: V, o: V): number {
    return 1000 + this.x * o.x;
  };
}
function restoreDot(): void {
  "use no aot";
  V.prototype.dot = savedDot as (o: V) => number;
}

for (const ints of [false, true]) {
  const bs = mk(7, ints);
  const runs: string[] = [];
  for (let r = 0; r < 4; r++) runs.push(advance(bs, 0.01));
  console.log(ints, runs.join(" "), pick(bs), last(bs, 1), last(bs, 2), sums(bs));
}

// Elements that are not what the regions need, midway.
const odd = mk(6, false) as unknown[];
odd[3] = { x: 1, y: 2, vx: 0, vy: "s", m: 1 };
console.log(advance(odd as B[], 0.01).slice(0, 12) === "NaN" ? "nan" : "num", pick(odd as B[]), last(odd as B[], 1));
const other = mk(5, false) as unknown[];
other[2] = { m: 1, x: 7.5, y: 1, vx: 0, vy: 0 };
console.log(advance(other as B[], 0.01), pick(other as B[]), sums(other as B[]));
const twice = mk(4, false);
twice[2] = twice[1];
console.log(advance(twice, 0.01), pick(twice), last(twice, 1));
const holes = mk(4, false);
holes.length = 6;
try {
  console.log(sums(holes));
} catch (e) {
  console.log((e as Error).constructor.name);
}
const sparse = mk(5, false) as unknown[];
delete sparse[2];
try {
  console.log(pick(sparse as B[]));
} catch (e) {
  console.log((e as Error).constructor.name);
}
const sentinel = mk(6, false);
sentinel[4].m = 99;
sentinel[1].x = -5;
console.log(pick(sentinel));
try {
  console.log(last(sentinel, 0.5 as number));
} catch (e) {
  console.log((e as Error).constructor.name);
}
console.log(partial(mk(3, false), mk(4, true)), partial(mk(4, true), mk(2, false)));
const shared = mk(3, false);
console.log(partial(shared, shared));

const sites = [new Site(new V(1, 2), 1.5), new Site(new V(-1, 4), 0.5), new Site(new V(3, -1), 2)];
const d = new V(0.5, 1.5);
console.log(nearest(sites, d), clamped([new V(1.5, 0), new V(-2, 0), new V(7, 1)], 0));
replaceDot();
console.log(nearest(sites, d));
restoreDot();
console.log(nearest(sites, d), nearest([...sites, new Site(new V(9, 0.5), 1)], d));
try {
  console.log(nearest([...sites, { at: { x: 9, y: 9 }, w: 1 } as unknown as Site], d));
} catch (e) {
  console.log((e as Error).constructor.name);
}
