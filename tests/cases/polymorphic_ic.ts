// @native area total scale nameOf sumX bump visit
// Property sites that see objects of several shapes (classes of a
// hierarchy, object literals with fields in other orders): each shape they
// meet is cached, up to a few, and the others are looked up each time; the
// results are those of the lookups, also after an object changes shape, a
// prototype's method is replaced, or a field becomes an accessor.

abstract class Shape {
  constructor(public name: string) {}
  abstract area(): number;
  describe(): string {
    return this.name + ":" + this.area().toFixed(2);
  }
}
class Circle extends Shape {
  constructor(public r: number) {
    super("circle");
  }
  area(): number {
    return Math.PI * this.r * this.r;
  }
}
class Rect extends Shape {
  constructor(public w: number, public h: number) {
    super("rect");
  }
  area(): number {
    return this.w * this.h;
  }
}
class Square extends Rect {
  constructor(s: number) {
    super(s, s);
    this.name = "square";
  }
}
class Tri extends Shape {
  constructor(public b: number, public h: number) {
    super("tri");
  }
  area(): number {
    return (this.b * this.h) / 2;
  }
}
class Dot extends Shape {
  constructor() {
    super("dot");
  }
  area(): number {
    return 0;
  }
}
class Ring extends Circle {
  constructor(r: number, public inner: number) {
    super(r);
    this.name = "ring";
  }
  area(): number {
    return super.area() - Math.PI * this.inner * this.inner;
  }
}

function area(s: Shape): number {
  return s.area();
}

function total(xs: Shape[]): number {
  let t = 0;
  for (const s of xs) t += s.area();
  return t;
}

function scale(xs: Shape[], k: number): void {
  for (const s of xs) {
    if (s instanceof Circle) s.r *= k;
    else if (s instanceof Rect) {
      s.w *= k;
      s.h *= k;
    }
  }
}

function nameOf(s: Shape): string {
  return s.name;
}

type HasX = { x: number };

function sumX(xs: HasX[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += xs[i].x;
  return s;
}

function bump(xs: HasX[], d: number): void {
  for (let i = 0; i < xs.length; i++) xs[i].x = xs[i].x + d;
}

function visit(xs: Shape[]): string {
  const out: string[] = [];
  for (const s of xs) out.push(s.describe());
  return out.join(" ");
}

const shapes: Shape[] = [];
for (let i = 1; i <= 14; i++) {
  switch (i % 7) {
    case 0: shapes.push(new Circle(i)); break;
    case 1: shapes.push(new Rect(i, 2)); break;
    case 2: shapes.push(new Square(i)); break;
    case 3: shapes.push(new Tri(i, 3)); break;
    case 4: shapes.push(new Dot()); break;
    case 5: shapes.push(new Ring(i, 1)); break;
    default: shapes.push(new Rect(1, i)); break;
  }
}
const r6 = (x: number) => Math.round(x * 1e6) / 1e6;
for (let round = 0; round < 3; round++) {
  console.log(r6(total(shapes)), shapes.map((s) => r6(area(s))).join(","));
  console.log(shapes.map(nameOf).join(","));
  scale(shapes, 1.5);
}
console.log(visit(shapes));

// Literals of many shapes (more than the cache holds), then changes.
const objs: HasX[] = [
  { x: 1 },
  { x: 2, y: 0 } as HasX,
  { y: 0, x: 3 } as HasX,
  { a: 1, b: 2, x: 4 } as HasX,
  { b: 2, x: 5, a: 1 } as HasX,
  { x: 6, z: "q" } as HasX,
  { q: null, r: 1, x: 7 } as HasX,
];
for (let round = 0; round < 3; round++) {
  console.log(sumX(objs));
  bump(objs, round + 0.5);
}
(objs[2] as any).w = 9; // another shape for a cached object
delete (objs[3] as any).a;
console.log(sumX(objs));
bump(objs, 1);
console.log(objs.map((o) => o.x).join(","));
let reads = 0;
Object.defineProperty(objs[1], "x", {
  get() {
    reads++;
    return 100;
  },
  set(v: number) {
    reads += v;
  },
});
console.log(sumX(objs), reads);
bump(objs, 2);
console.log(sumX(objs), reads);
Object.freeze(objs[4]);
try {
  bump(objs, 1);
} catch (e) {
  console.log("frozen", (e as Error).constructor.name);
}
console.log(objs.map((o) => o.x).join(","));

// A method replaced on one prototype; one shadowed by an own property.
Rect.prototype.area = function (this: Rect) {
  return -this.w;
};
console.log(r6(total(shapes)));
(shapes[3] as any).area = () => 1000;
console.log(r6(total(shapes)), area(shapes[3]), area(shapes[10]));
console.log(visit(shapes.slice(0, 7)));
