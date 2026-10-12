// @native trace firstHit
// @interpreted replaceSub restoreSub
// A loop region over a ray tracer's spheres: each iteration runs
// `spheres[i].hit(origin, dir)` inline, whose body makes a vector
// (scalar-replaced), passes its parameters on to other inlined methods and
// returns early. The vector's fields, set again in each iteration, need
// nothing from the guard; an element of another shape midway, a sphere
// whose center is a plain object, ints in the fields and a replaced method
// take the original loop, with the same results.

class Vec3 {
  x: number;
  y: number;
  z: number;
  constructor(x: number, y: number, z: number) {
    this.x = x;
    this.y = y;
    this.z = z;
  }
  sub(o: Vec3): Vec3 {
    return new Vec3(this.x - o.x, this.y - o.y, this.z - o.z);
  }
  dot(o: Vec3): number {
    return this.x * o.x + this.y * o.y + this.z * o.z;
  }
}

class Sphere {
  center: Vec3;
  radius: number;
  constructor(center: Vec3, radius: number) {
    this.center = center;
    this.radius = radius;
  }
  hit(origin: Vec3, dir: Vec3): number {
    const oc = origin.sub(this.center);
    const b = oc.dot(dir);
    const c = oc.dot(oc) - this.radius * this.radius;
    const disc = b * b - c;
    if (disc < 0) return -1;
    const t = -b - Math.sqrt(disc);
    return t > 1e-4 ? t : -1;
  }
}

function trace(spheres: Sphere[], origin: Vec3, dir: Vec3): string {
  let best = -1;
  let bestT = Infinity;
  for (let i = 0; i < spheres.length; i++) {
    const t = spheres[i].hit(origin, dir);
    if (t > 0 && t < bestT) {
      bestT = t;
      best = i;
    }
  }
  return best < 0 ? "miss" : best + ":" + bestT.toFixed(6);
}

// The first sphere hit, leaving the loop with `break`.
function firstHit(spheres: Sphere[], origin: Vec3, dir: Vec3): string {
  let found = -1;
  let at = 0;
  for (let i = 0; i < spheres.length; i++) {
    const t = spheres[i].hit(origin, dir);
    if (t > 0) {
      found = i;
      at = t;
      break;
    }
  }
  return found + ":" + at.toFixed(4);
}

let saved: unknown;
function replaceSub(): void {
  "use no aot";
  saved = Vec3.prototype.sub;
  Vec3.prototype.sub = function (this: Vec3, o: Vec3): Vec3 {
    return new Vec3(this.x - o.x + 0.5, this.y - o.y, this.z - o.z);
  };
}
function restoreSub(): void {
  "use no aot";
  Vec3.prototype.sub = saved as (o: Vec3) => Vec3;
}

const spheres = [
  new Sphere(new Vec3(0, -1000, 0), 999),
  new Sphere(new Vec3(-1.5, 0, 4), 1),
  new Sphere(new Vec3(1.5, 0, 5), 1),
  new Sphere(new Vec3(0, 1.5, 6), 1),
];
const eye = new Vec3(0, 0.5, -2);
const rays: Vec3[] = [];
for (let y = 0; y < 4; y++) for (let x = 0; x < 4; x++) rays.push(new Vec3((x - 2) / 4, (2 - y) / 4, 1));
const run = (ss: Sphere[]) => rays.map((d) => trace(ss, eye, d) + "/" + firstHit(ss, eye, d)).join(" ");
console.log(run(spheres));

// Another shape midway; a center that is a plain object; fractional
// fields everywhere.
const odd = [...spheres] as unknown[];
const other = Object.create(Sphere.prototype) as Sphere;
other.radius = 1;
other.center = new Vec3(1.5, 0, 5);
odd[2] = other;
console.log(run(odd as Sphere[]));
const plain = [...spheres];
plain[1] = new Sphere({ x: -1.5, y: 0, z: 4 } as Vec3, 1);
console.log(run(plain));
const floats = spheres.map((s) => new Sphere(new Vec3(s.center.x + 0.25, s.center.y + 0.5, s.center.z + 0.125), s.radius + 0.5));
console.log(run(floats));

replaceSub();
console.log(run(spheres));
restoreSub();
console.log(run(spheres));
