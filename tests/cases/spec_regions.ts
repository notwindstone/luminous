// @native advance energy count alias negZero swap kick inlined bounce clampAll pick settle
// Speculative numeric regions: straight-line arithmetic on number fields
// of annotated object types runs as float code behind one guard (shapes,
// number tags, distinct objects); anything else (ints, strings, BigInts,
// the same object twice, accessors, frozen objects) takes the generic
// code, with the same results.

type P = { x: number; y: number; vx: number; vy: number; m: number };
type C = { n: number; total: number; label: string };

function make(x: number, y: number, m: number): P {
  return { x, y, vx: 0, vy: 0, m };
}

function advance(ps: P[], dt: number): void {
  for (let i = 0; i < ps.length; i++) {
    const a = ps[i];
    for (let j = i + 1; j < ps.length; j++) {
      const b = ps[j];
      const dx = a.x - b.x;
      const dy = a.y - b.y;
      const d2 = dx * dx + dy * dy + 0.01;
      const f = dt / (d2 * Math.sqrt(d2));
      a.vx -= dx * b.m * f;
      a.vy -= dy * b.m * f;
      b.vx += dx * a.m * f;
      b.vy += dy * a.m * f;
    }
  }
  for (const p of ps) {
    p.x += dt * p.vx;
    p.y += dt * p.vy;
  }
}

function energy(ps: P[]): number {
  let e = 0;
  for (const p of ps) {
    const v2 = p.vx * p.vx + p.vy * p.vy;
    e += 0.5 * p.m * v2;
  }
  return e;
}

// Int fields stay ints; a string or a BigInt sends the region to the
// generic code (concatenation, a TypeError).
function count(cs: C[], k: number): string {
  for (const c of cs) {
    c.n = c.n + k;
    c.total = c.total * 2 + c.n;
  }
  return cs.map((c) => [c.n, c.total, c.label, Number.isInteger(c.n)].join("/")).join(" ");
}

// The same object as both operands: writes through one are seen by the other.
function alias(a: P, b: P): string {
  a.x = a.x + 1;
  b.x = b.x * 10;
  a.y = b.x + a.x;
  b.y = a.y - b.y;
  return [a.x, a.y, b.x, b.y].join(",");
}

function negZero(p: P): string {
  p.vx = p.x * -1;
  p.vy = p.y * p.vx;
  const z = p.vx;
  return [Object.is(p.vx, -0), Object.is(z, -0), p.vy, 1 / p.vx].join(" ");
}

// Locals read before they are assigned, and assigned for later.
function swap(p: P): string {
  let t: number | string = p.x;
  let s = 0;
  for (let i = 0; i < 3; i++) {
    t = p.y + p.x * 2;
    p.y = p.x;
    p.x = t;
    s = s + t * p.m;
  }
  return [t, s, p.x, p.y].join(" ");
}

class Ball {
  x: number;
  v: number;
  constructor(x: number, v: number) {
    this.x = x;
    this.v = v;
  }
  step(dt: number): void {
    this.v = this.v - 9.81 * dt;
    this.x = this.x + this.v * dt;
  }
}

function kick(bs: Ball[], dt: number): string {
  for (const b of bs) {
    b.v = b.v * 0.5 + dt;
    b.x = b.x + b.v;
  }
  return bs.map((b) => b.x.toFixed(4) + ":" + b.v.toFixed(4)).join(" ");
}

function inlined(b: Ball, n: number): string {
  for (let i = 0; i < n; i++) b.step(0.01);
  return b.x.toFixed(6) + " " + b.v.toFixed(6);
}

// Branches, early returns and conditional writes inside a region.
function bounce(p: P, w: number): number {
  p.x = p.x + p.vx;
  if (p.x < 0) {
    p.x = -p.x;
    p.vx = -p.vx;
    return 1;
  }
  if (p.x > w) {
    p.x = 2 * w - p.x;
    p.vx = -p.vx * p.m;
  }
  return p.x > w / 2 ? p.vx * 2 + p.m : p.vy || p.x * 3;
}

function clampAll(ps: P[], lo: number, hi: number): string {
  let hits = 0;
  for (const p of ps) {
    let d;
    if (p.y < lo) {
      d = lo - p.y;
      p.y = lo;
    } else if (p.y > hi) {
      d = p.y - hi;
      p.y = hi;
    } else {
      d = 0;
    }
    p.vy = p.vy * (d > 0 ? -0.5 : 1) + d * p.m;
    hits = hits + (d > 0 ? 1 : 0);
  }
  return hits + " " + ps.map((p) => p.y + ":" + p.vy).join(",");
}

function pick(a: P, b: P): number {
  const da = a.x * a.x + a.y * a.y - a.m * 2;
  const db = b.x * b.x + b.y * b.y - b.m * 2;
  return da < db ? da - db * 0.5 : (a.vx - b.vx) * 3 || a.vy + b.vy;
}

// A loop inside a region: fields carried from one iteration to the
// next, a write under a condition, an early return.
function settle(p: P, steps: number): number {
  let bounces = 0;
  for (let k = 0; k < steps; k++) {
    if (p.y <= 0) {
      p.y = 0;
      p.vy = -p.vy * 0.5;
      bounces = bounces + 1;
      if (p.vy < 0.5) return k * 100 + bounces;
    }
    p.vy = p.vy - 9.81 * 0.1;
    p.y = p.y + p.vy * 0.1;
  }
  return -bounces;
}

const ps = [make(0, 0, 10), make(1, 0, 1), make(0, 2, 1), make(3, 3, 0.5)];
for (let k = 0; k < 200; k++) advance(ps, 0.01);
console.log(energy(ps).toFixed(9), ps.map((p) => p.x.toFixed(6) + "," + p.y.toFixed(6)).join(" "));

// Not numbers: a string field, a missing field, NaN.
const odd = [make(0, 0, 1), { x: "1", y: 2, vx: 0, vy: 0, m: 1 } as unknown as P, make(NaN, 1, 1)];
advance(odd, 0.5);
console.log(JSON.stringify(odd));
const partial = [make(1, 1, 1), { x: 5, y: 5, m: 1 } as unknown as P];
advance(partial, 0.5);
console.log(JSON.stringify(partial));

const cs: C[] = [
  { n: 1, total: 0, label: "a" },
  { n: 2.5, total: 1, label: "b" },
  { n: 2147483647, total: 0, label: "c" },
];
console.log(count(cs, 1));
console.log(count(cs, 0.5));
const weird = [{ n: "7", total: 1, label: "s" } as unknown as C, { n: 1, total: 2, label: "t" }];
console.log(count(weird, 1));
try {
  count([{ n: 1n, total: 0, label: "big" } as unknown as C], 1);
} catch (e) {
  console.log((e as Error).constructor.name);
}

const q = make(2, 3, 1);
console.log(alias(q, q), alias(make(2, 3, 1), make(2, 3, 1)));
console.log(negZero(make(0, 5, 1)), negZero(make(-0, 5, 1)), negZero(make(2, 5, 1)));
console.log(swap(make(1, 2, 3)), swap(make(0.5, 0.25, 1)));

// Accessors and frozen objects: the guard fails, the generic code runs.
let reads = 0;
const g = {
  get x() {
    reads++;
    return 4;
  },
  y: 1,
  vx: 0,
  vy: 0,
  m: 1,
} as unknown as P;
console.log(energy([g, make(1, 1, 2)]), reads, negZero(make(3, 3, 3)));
const fz = Object.freeze(make(1, 2, 3));
try {
  advance([make(0, 0, 1), fz], 0.1);
} catch (e) {
  console.log((e as Error).constructor.name, JSON.stringify(fz));
}

const balls = [new Ball(0, 1), new Ball(10, -2), new Ball(1, 0)];
console.log(kick(balls, 0.25), kick(balls, 1));
console.log(inlined(new Ball(100, 0), 50), inlined(new Ball(0, 3), 10));
const bs = [make(1, 0, 2), make(9, 1, 1), make(5, 0, 3), { x: "4", y: 0, vx: 1, vy: 0, m: 1 } as unknown as P];
bs[0].vx = -3;
bs[1].vx = 4;
bs[2].vx = 0.5;
console.log(bs.map((p) => bounce(p, 10)).join(" "), JSON.stringify(bs));
console.log(clampAll([make(0, -5, 1), make(0, 3, 2), make(0, 12.5, 0.5), make(0, 7, 1)], 0, 10));
console.log(pick(make(1, 2, 1), make(3, 4, 1)), pick(make(3, 4, 1), make(1, 2, 1)), pick(make(2, 2, 4), make(2, 2, 4)), pick(make(1, 1, 1), q));
const fall = make(0, 10, 1);
const short = make(0, 3, 1);
console.log(settle(fall, 1000), JSON.stringify(fall), settle(short, 5), JSON.stringify(short));
