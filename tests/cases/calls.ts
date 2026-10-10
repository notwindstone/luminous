// @native apply twice callMethod construct isPoint viaInterp defaults optional
class Point {
  constructor(public x: number, public y: number) {}
  sum(): number { return this.x + this.y; }
}
function interpHelper(a: number, b: string): string { return b.repeat(a); }

aot function apply(f: (x: number) => number, x: number): unknown {
  return f(x);
}
aot function twice(x: number): number {
  return x * 2;
}
aot function callMethod(p: Point): number {
  return p.sum() * 10;
}
aot function construct(x: number): Point {
  return new Point(x, x + 1);
}
aot function isPoint(v: unknown): boolean {
  return v instanceof Point;
}
// Native -> interpreted -> native.
aot function viaInterp(n: number): string {
  return interpHelper(n, "ab") + apply(twice, n);
}
aot function defaults(a: number, b = 10, c: string = "x" + b): string {
  return a + "/" + b + "/" + c;
}
aot function optional(a: number, b?: number): string {
  return b === undefined ? "none" : "b=" + b;
}

console.log(apply((x) => x + 1, 41), apply(twice, 21), apply(Math.sqrt, 16));
console.log(callMethod(new Point(1, 2)), JSON.stringify(construct(5)), construct(5).sum());
console.log(isPoint(new Point(0, 0)), isPoint({ x: 0, y: 0 }), viaInterp(3));
console.log(defaults(1), defaults(1, 2), defaults(1, undefined, "z"), optional(1), optional(1, 2));
try {
  apply(undefined as never, 1);
} catch (e) {
  console.log((e as Error).name);
}
