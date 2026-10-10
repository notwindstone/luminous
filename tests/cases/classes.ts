// @native norm add scale describe origin swap twice
// @interpreted secret mapped
class Vec2 {
  #hidden = 7;
  constructor(public x: number, public y: number) {}
  norm(): number {
    return Math.sqrt(this.x * this.x + this.y * this.y);
  }
  add(o: Vec2): Vec2 {
    return new Vec2(this.x + o.x, this.y + o.y);
  }
  scale(k: number): this {
    this.x *= k;
    this.y *= k;
    return this;
  }
  describe(): string {
    return `Vec2(${this.x}, ${this.y}) |${this.norm().toFixed(3)}|`;
  }
  static origin(): Vec2 {
    return new Vec2(0, 0);
  }
  swap(): void {
    const t = this.x;
    this.x = this.y;
    this.y = t;
  }
  // Private fields are not supported natively yet: stays interpreted.
  secret(): number {
    return this.#hidden;
  }
  // Arrow functions capturing `this` are not supported natively yet.
  mapped(xs: number[]): number[] {
    return xs.map((v) => v * this.x);
  }
  get length(): number {
    return this.norm();
  }
}

class Vec3 extends Vec2 {
  constructor(x: number, y: number, public z: number) {
    super(x, y);
  }
  // Overrides a native method; calls it through `super`.
  describe(): string {
    return super.describe() + " z=" + this.z;
  }
}

class Counter {
  count = 0;
  twice(step: number): number {
    this.count += step;
    this.count += step;
    return this.count;
  }
}

const a = new Vec2(3, 4);
const b = new Vec2(1, 2);
console.log(a.norm(), a.add(b).describe(), Vec2.origin().describe(), a.length);
console.log(a.scale(2).describe(), a.secret(), a.mapped([1, 2]).join(","));
a.swap();
console.log(a.describe(), new Vec3(1, 2, 3).describe(), new Vec3(1, 2, 3).norm());
// Methods keep their attributes and work with any receiver.
console.log(Object.keys(Vec2.prototype).length, Vec2.prototype.norm.name, Vec2.prototype.add.length);
console.log(Object.getOwnPropertyDescriptor(Vec2.prototype, "norm")!.enumerable, typeof Vec2.origin);
const norm = Vec2.prototype.norm;
console.log(norm.call({ x: 6, y: 8 }), norm.call({ x: "6", y: 8 } as never));
// Ill-typed arguments fall back to the interpreted method.
const c = new Counter();
console.log(c.twice(1), c.twice("x" as never), c.count);
try {
  norm.call(undefined as never);
} catch (e) {
  console.log((e as Error).name);
}
