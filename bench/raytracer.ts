// A small ray tracer: classes with methods, vector math on fresh objects.
class Vec3 {
  x: number;
  y: number;
  z: number;
  constructor(x: number, y: number, z: number) {
    this.x = x;
    this.y = y;
    this.z = z;
  }
  add(o: Vec3): Vec3 {
    return new Vec3(this.x + o.x, this.y + o.y, this.z + o.z);
  }
  sub(o: Vec3): Vec3 {
    return new Vec3(this.x - o.x, this.y - o.y, this.z - o.z);
  }
  scale(k: number): Vec3 {
    return new Vec3(this.x * k, this.y * k, this.z * k);
  }
  dot(o: Vec3): number {
    return this.x * o.x + this.y * o.y + this.z * o.z;
  }
  norm(): Vec3 {
    return this.scale(1 / Math.sqrt(this.dot(this)));
  }
}

class Sphere {
  center: Vec3;
  radius: number;
  color: Vec3;
  constructor(center: Vec3, radius: number, color: Vec3) {
    this.center = center;
    this.radius = radius;
    this.color = color;
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

function trace(spheres: Sphere[], origin: Vec3, dir: Vec3, light: Vec3, depth: number): Vec3 {
  let best = -1;
  let bestT = Infinity;
  for (let i = 0; i < spheres.length; i++) {
    const t = spheres[i].hit(origin, dir);
    if (t > 0 && t < bestT) {
      bestT = t;
      best = i;
    }
  }
  if (best < 0) return new Vec3(0.2, 0.3, 0.5);
  const s = spheres[best];
  const p = origin.add(dir.scale(bestT));
  const n = p.sub(s.center).norm();
  const diffuse = Math.max(0, n.dot(light));
  let color = s.color.scale(0.2 + 0.8 * diffuse);
  if (depth > 0) {
    const r = dir.sub(n.scale(2 * dir.dot(n)));
    color = color.add(trace(spheres, p, r, light, depth - 1).scale(0.3));
  }
  return color;
}

function render(w: number, h: number): number {
  const spheres = [
    new Sphere(new Vec3(0, -1000, 0), 999, new Vec3(0.5, 0.5, 0.5)),
    new Sphere(new Vec3(-1.5, 0, 4), 1, new Vec3(0.9, 0.2, 0.2)),
    new Sphere(new Vec3(1.5, 0, 5), 1, new Vec3(0.2, 0.9, 0.2)),
    new Sphere(new Vec3(0, 1.5, 6), 1, new Vec3(0.2, 0.2, 0.9)),
  ];
  const light = new Vec3(1, 1, -1).norm();
  const eye = new Vec3(0, 0.5, -2);
  let sum = 0;
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const dir = new Vec3((x - w / 2) / w, (h / 2 - y) / h, 1).norm();
      const c = trace(spheres, eye, dir, light, 3);
      sum += c.x + c.y + c.z;
    }
  }
  return sum;
}

const start = Date.now();
console.log(`checksum: ${render(320, 240).toFixed(6)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
