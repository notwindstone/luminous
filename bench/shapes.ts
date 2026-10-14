// Polymorphic class hierarchy: virtual calls, getters, inheritance.
abstract class Shape {
  constructor(public x: number, public y: number) {}
  abstract area(): number;
  abstract perimeter(): number;
  move(dx: number, dy: number): void { this.x += dx; this.y += dy; }
  describe(): string { return `${this.kind()}@${this.x.toFixed(1)},${this.y.toFixed(1)}`; }
  abstract kind(): string;
}
class Circle extends Shape {
  constructor(x: number, y: number, public r: number) { super(x, y); }
  area(): number { return Math.PI * this.r * this.r; }
  perimeter(): number { return 2 * Math.PI * this.r; }
  kind(): string { return "circle"; }
}
class Rect extends Shape {
  constructor(x: number, y: number, public w: number, public h: number) { super(x, y); }
  area(): number { return this.w * this.h; }
  perimeter(): number { return 2 * (this.w + this.h); }
  kind(): string { return "rect"; }
}
class Square extends Rect {
  constructor(x: number, y: number, s: number) { super(x, y, s, s); }
  kind(): string { return "square"; }
}
const start = Date.now();
const shapes: Shape[] = [];
for (let i = 0; i < 200000; i++) {
  const k = i % 3;
  shapes.push(k === 0 ? new Circle(i, -i, 1 + (i % 7)) : k === 1 ? new Rect(i, i, 2 + (i % 5), 3) : new Square(-i, i, 1 + (i % 4)));
}
let a = 0, p = 0, len = 0;
for (let round = 0; round < 20; round++) {
  for (const s of shapes) {
    a += s.area();
    p += s.perimeter();
    s.move(0.5, -0.25);
  }
  if (round % 5 === 0) for (let i = 0; i < shapes.length; i += 97) len += shapes[i].describe().length;
}
console.log(`area ${a.toFixed(2)} perim ${p.toFixed(2)} len ${len}`);
console.log(`elapsed: ${Date.now() - start} ms`);
