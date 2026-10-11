// Allocation-heavy code with small objects, closures and arrays of objects:
// object literals, property access, callbacks.
type Point = { x: number; y: number };

function makePoints(n: number): Point[] {
  const ps: Point[] = [];
  for (let i = 0; i < n; i++) ps.push({ x: i % 100, y: (i * 7) % 100 });
  return ps;
}

function centroid(ps: Point[]): Point {
  let sx = 0, sy = 0;
  for (const p of ps) {
    sx += p.x;
    sy += p.y;
  }
  return { x: sx / ps.length, y: sy / ps.length };
}

function translate(ps: Point[], dx: number, dy: number): Point[] {
  return ps.map((p) => ({ x: p.x + dx, y: p.y + dy }));
}

function countNear(ps: Point[], c: Point, r: number): number {
  return ps.filter((p) => (p.x - c.x) ** 2 + (p.y - c.y) ** 2 < r * r).length;
}

const start = Date.now();
let total = 0;
for (let round = 0; round < 20; round++) {
  let ps = makePoints(100_000);
  for (let k = 0; k < 5; k++) ps = translate(ps, 1, -1);
  const c = centroid(ps);
  total += countNear(ps, c, 30);
}
console.log(`total: ${total}`);
console.log(`elapsed: ${Date.now() - start} ms`);
