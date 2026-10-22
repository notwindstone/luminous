// @native clamp near closest cmp zeros
// Escape clones whose parameters became numbers: their locals that only
// hold numbers (a ternary of numbers too) are numbers, and comparisons of
// boxed numbers number comparisons, NaN and -0 included.
type P = { x: number; y: number };

function clamp(p: P, lo: P, hi: P): number {
  const span = hi.x - lo.x;
  let t = span > 0 ? (p.x - lo.x) / span : 0;
  if (t < 0) t = 0;
  else if (t > 1) t = 1;
  const d = p.y - t * hi.y;
  return Math.sqrt(d * d + t);
}

function near(p: P, q: P): boolean {
  for (let i = 0; i < 2; i++) if (p.x == q.x) return true;
  return p.y === q.y;
}

function cmp(a: P, b: P): string {
  let out = "";
  for (let i = 0; i < 1; i++) {
    out += (a.x < b.x ? "<" : "") + (a.x > b.x ? ">" : "") + (a.x <= b.x ? "le" : "") + (a.x >= b.x ? "ge" : "");
    out += (a.x == b.x ? "eq" : "") + (a.x != b.x ? "ne" : "") + (a.x === b.x ? "EQ" : "") + (a.x !== b.x ? "NE" : "");
  }
  return out;
}

function zeros(a: P): string {
  let s = "";
  for (let i = 0; i < 1; i++) {
    const z = a.x * -1;
    s += (z === 0 ? "zero " : "nonzero ") + (1 / z) + " " + Object.is(z, -0);
  }
  return s;
}

function closest(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const p = { x: i * 0.37, y: (i % 5) - 2 };
    const lo = { x: 0, y: 0 };
    const hi = { x: (i % 3) * 2, y: 1.5 };
    total += clamp(p, lo, hi);
    if (near(p, lo)) total += 1;
  }
  return total;
}

console.log(closest(50).toFixed(6));
const nan = { x: NaN, y: 0 };
const one = { x: 1, y: 0 };
const zero = { x: 0, y: -0 };
console.log(cmp(nan, one), cmp(one, nan), cmp(nan, nan), cmp(one, one), cmp(zero, { x: -0, y: 0 }));
console.log(near(nan, nan), near(zero, { x: -0, y: 0 }), near({ x: 1, y: NaN }, { x: 2, y: NaN }));
console.log(zeros(zero), zeros(one), zeros(nan));
