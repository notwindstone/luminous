export class Vec {
  constructor(public x: number, public y: number) {}
  add(o: Vec): Vec { return new Vec(this.x + o.x, this.y + o.y); }
  len(): number { return Math.sqrt(this.x * this.x + this.y * this.y); }
}
export function dot(a: Vec, b: Vec): number { return a.x * b.x + a.y * b.y; }
function helper(): string { return "vec-helper"; }
export const ORIGIN = new Vec(0, 0);
console.log("vec loaded", helper());
