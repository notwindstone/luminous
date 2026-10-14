// @native Vec scale dot sum lengths walk wrong patched subclass fieldHints
// Type annotations naming classes predict the methods calls reach (checked
// at each call): parameters, variables, fields, array elements and the
// results of annotated methods. Wrong annotations only cost the check.

class Vec {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  scale(k: number): Vec {
    return new Vec(this.x * k, this.y * k);
  }
  dot(o: Vec): number {
    return this.x * o.x + this.y * o.y;
  }
  len(): number {
    return Math.sqrt(this.dot(this));
  }
}

class Tagged extends Vec {
  tag = "t";
  dot(o: Vec): number {
    return -super.dot(o);
  }
}

class Line {
  from: Vec;
  to: Vec;
  next: Line | null = null;
  constructor(from: Vec, to: Vec) {
    this.from = from;
    this.to = to;
  }
  length(): number {
    return this.to.scale(1).dot(this.from) + this.from.len();
  }
}

function scale(v: Vec, k: number): number {
  return v.scale(k).len();
}

function dot(a: Vec, b: Vec): number {
  const c = a.scale(2);
  const d: Vec = b.scale(0.5);
  return c.dot(d) + a.dot(b);
}

function sum(vs: Vec[]): number {
  let s = 0;
  for (let i = 0; i < vs.length; i++) s += vs[i].dot(vs[i]) + vs[i].scale(3).x;
  return s;
}

function lengths(ls: Line[]): string {
  return ls.map((l) => l.length().toFixed(3)).join(",");
}

function walk(l: Line): number {
  let n = 0;
  for (let c: Line | null = l; c !== null; c = c.next) n += c.from.dot(c.to);
  return n;
}

function wrong(v: Vec): string {
  // Annotated `Vec`, given other objects.
  try {
    return String(v.scale(2).len());
  } catch (e) {
    return (e as Error).constructor.name;
  }
}

function patched(v: Vec): number {
  return v.dot(v);
}

function subclass(v: Vec, w: Vec): number {
  return v.dot(w) + w.scale(2).dot(v);
}

function fieldHints(l: Line): number {
  return l.from.x + l.to.scale(10).y;
}

const a = new Vec(3, 4);
const b = new Vec(1, 2);
console.log(scale(a, 2), dot(a, b), sum([a, b, new Vec(5, 6)]));
const l1 = new Line(a, b);
const l2 = new Line(b, new Vec(7, 8));
l1.next = l2;
console.log(lengths([l1, l2]), walk(l1), fieldHints(l1));
const fake = {
  scale(k: number) {
    return { len: () => k * 100 };
  },
} as unknown as Vec;
console.log(wrong(a), wrong(fake), wrong(null as unknown as Vec), wrong({} as Vec));
console.log(patched(a));
const orig = Vec.prototype.dot;
Vec.prototype.dot = function (this: Vec, o: Vec): number {
  return 1000 + orig.call(this, o);
};
console.log(patched(a), scale(a, 1), dot(a, b));
Vec.prototype.dot = orig;
console.log(patched(a), subclass(a, b), subclass(new Tagged(1, 1), b), subclass(b, new Tagged(2, 3)));
