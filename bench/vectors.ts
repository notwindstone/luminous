// Points and vectors made in hot loops and passed to helper functions:
// small ones (inlined) and larger ones (not inlined: called with the
// vectors as arguments), as object literals and as class instances.
type Vec = { x: number; y: number; z: number };

function lengthSq(v: Vec): number {
  return v.x * v.x + v.y * v.y + v.z * v.z;
}

// Distance from point p to segment ab.
function segmentDistance(p: Vec, a: Vec, b: Vec): number {
  const abx = b.x - a.x, aby = b.y - a.y, abz = b.z - a.z;
  const apx = p.x - a.x, apy = p.y - a.y, apz = p.z - a.z;
  const len = abx * abx + aby * aby + abz * abz;
  let t = len > 0 ? (apx * abx + apy * aby + apz * abz) / len : 0;
  if (t < 0) t = 0;
  else if (t > 1) t = 1;
  const dx = apx - t * abx, dy = apy - t * aby, dz = apz - t * abz;
  return Math.sqrt(dx * dx + dy * dy + dz * dz);
}

// Distance to the nearer of two segments a-b and b-c.
function nearest(p: Vec, a: Vec, b: Vec, c: Vec): number {
  const d1 = segmentDistance(p, a, b);
  const d2 = segmentDistance(p, b, c);
  return d1 < d2 ? d1 : d2;
}

function literals(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const t = i * 0.001;
    const p = { x: Math.sin(t), y: Math.cos(t), z: t % 1 };
    const a = { x: 0, y: 0, z: 0 };
    const b = { x: 1, y: (i & 7) * 0.25, z: 0.5 };
    const c = { x: 2, y: 1, z: (i & 3) * 0.5 };
    total += nearest(p, a, b, c) + lengthSq(p);
  }
  return total;
}

class V3 {
  constructor(
    public x: number,
    public y: number,
    public z: number,
  ) {}
  dot(o: V3): number {
    return this.x * o.x + this.y * o.y + this.z * o.z;
  }
}

// Not inlined (a loop): the instances' fields read through parameters.
function project(v: V3, axis: V3, steps: number): number {
  let s = 0;
  for (let k = 0; k < steps; k++) s += (v.x * axis.x + v.y * axis.y + v.z * axis.z) * (k + 1);
  return s;
}

function instances(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const t = i * 0.001;
    const v = new V3(Math.sin(t), Math.cos(t), t % 1);
    const axis = new V3(1, (i & 7) * 0.25, 0.5);
    total += project(v, axis, 2) + v.dot(axis);
  }
  return total;
}

const start = Date.now();
let a = 0;
let b = 0;
for (let round = 0; round < 5; round++) {
  a += literals(1_000_000);
  b += instances(1_000_000);
}
console.log(`literals: ${a.toFixed(3)}`);
console.log(`instances: ${b.toFixed(3)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
