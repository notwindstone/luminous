// @native hit scan shade lengths
// @interpreted replaceDot restoreDot
// Numeric regions through methods: `this`'s fields, objects held by
// fields (`this.center`), predicted method calls run inline, and the
// scalar-replaced vectors they return, as float code behind one guard.
// A replaced method, a field holding something else, strings, another
// prototype, ints and odd numbers take the generic code, with the same
// results.

class V {
  x: number;
  y: number;
  z: number;
  constructor(x: number, y: number, z: number) {
    this.x = x;
    this.y = y;
    this.z = z;
  }
  add(o: V): V {
    return new V(this.x + o.x, this.y + o.y, this.z + o.z);
  }
  sub(o: V): V {
    return new V(this.x - o.x, this.y - o.y, this.z - o.z);
  }
  scale(k: number): V {
    return new V(this.x * k, this.y * k, this.z * k);
  }
  dot(o: V): number {
    return this.x * o.x + this.y * o.y + this.z * o.z;
  }
}

class Ball {
  center: V;
  radius: number;
  constructor(center: V, radius: number) {
    this.center = center;
    this.radius = radius;
  }
  hit(origin: V, dir: V): number {
    const oc = origin.sub(this.center);
    const b = oc.dot(dir);
    const c = oc.dot(oc) - this.radius * this.radius;
    const disc = b * b - c;
    if (disc < 0) return -1;
    const t = -b - Math.sqrt(disc);
    return t > 1e-4 ? t : -1;
  }
}

function hit(b: Ball, o: V, d: V): number {
  return b.hit(o, d);
}

function scan(balls: Ball[], o: V, d: V): string {
  const out: string[] = [];
  for (let i = 0; i < balls.length; i++) {
    const t = balls[i].hit(o, d);
    out.push(typeof t === "number" ? t.toFixed(6) : String(t));
  }
  return out.join(" ");
}

function shade(o: V, d: V, t: number): string {
  const p = o.add(d.scale(t));
  const q = p.sub(o);
  return [p.x, p.y, p.z, q.dot(q)].map((v) => (typeof v === "number" ? v.toFixed(4) : String(v))).join(",");
}

function lengths(vs: V[]): string {
  return vs.map((v) => v.dot(v)).join(",");
}

let savedDot: unknown;
let calls = 0;
function replaceDot(): void {
  "use no aot";
  savedDot = V.prototype.dot;
  V.prototype.dot = function (this: V, o: V): number {
    calls++;
    return 1000 + this.x * o.x;
  };
}

function restoreDot(): void {
  "use no aot";
  V.prototype.dot = savedDot as (o: V) => number;
}

const o = new V(0, 0, -5);
const d = new V(0, 0, 1);
const balls = [new Ball(new V(0, 0, 0), 1), new Ball(new V(0.5, 0, 3), 2), new Ball(new V(10, 10, 0), 1)];
console.log(scan(balls, o, d), hit(balls[0], o, d));
console.log(shade(o, d, 2.5), lengths([o, d, new V(1.5, -2, 0.25)]));

// A center that is not a V (another shape), a string field, ints, NaN.
const odd = [
  new Ball({ x: 0, y: 0, z: 0, w: 1 } as unknown as V, 1),
  new Ball(new V(0, 0, 0), "1" as unknown as number),
  new Ball(new V(0, 0, 1), 3),
  new Ball(new V(NaN, 0, 0), 1),
  new Ball(new V(0, 0, 0), -0),
];
console.log(scan(odd, o, d));
console.log(scan(balls, new V("0" as unknown as number, 0, -5), d), shade(new V(1, 2, 3), new V(1, 1, 1), 2));

// Another prototype with the same fields.
class W {
  x = 0;
  y = 0;
  z = 0;
  sub(): V {
    return new V(7, 7, 7);
  }
  dot(): number {
    return 42;
  }
}
const w = new W();
console.log(scan(balls, w as unknown as V, d), lengths([w as unknown as V]));

// A replaced method: the region's guard fails, the new one runs.
replaceDot();
console.log(scan(balls, o, d), lengths([d]), calls);
restoreDot();
console.log(scan(balls, o, d), lengths([d]), calls);
