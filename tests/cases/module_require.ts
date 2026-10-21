// import.meta.require("..."): a synchronous import (Bun's, for the CommonJS
// `require` in ES modules). A module only it imports is evaluated when the
// first call runs, once; the call returns the namespace object. Also what
// TypeScript only declares: overload signatures, `declare class X` merged
// with `function X`, `declare namespace X` merged with `function X`, and
// `import type` of what does not resolve. Abstract classes in a lazy module.
import type { Nothing } from "no-such-package";
import { eager } from "./module_require/eager.ts";

console.log("main start");

function lazy() {
  return import.meta.require("./module_require/lazy.ts");
}

console.log("before the first call");
const ns = lazy();
console.log("value:", ns.value, ns.default(2), ns.counter);
ns.bump();
console.log("live:", ns.counter, "same namespace:", ns === lazy());
console.log("names:", ns.fmt.name, ns.fmt(), ns.Point.name, ns.n.name);
console.log("eager:", import.meta.require("./module_require/eager.ts").eager === eager);

// A module that throws: every call throws its error.
for (let i = 0; i < 2; i++) {
  try {
    import.meta.require("./module_require/fails.ts");
  } catch (e) {
    console.log("fails:", (e as Error).message);
  }
}

// Overload signatures.
function fmt(x: string): string;
function fmt(x: number): string;
function fmt(x: unknown): string {
  return `<${String(x)}>`;
}
console.log(fmt("a"), fmt(1));

// `declare class` merged with a function of the same name.
declare class Point {
  x: number;
  constructor(x: number);
}
function Point(this: any, x: number) {
  this.x = x;
}
Point.prototype.double = function (this: any) {
  return this.x * 2;
};
Object.defineProperty(Point, "label", { value: "Point" });
const p = new (Point as any)(21);
console.log(p.double(), (Point as any).label);

const n: Nothing | undefined = undefined;
console.log("type-only import:", n);

function shapes() {
  return import.meta.require("./module_require/shapes.ts");
}
const sq = new (shapes().Square)(3);
console.log(sq.describe(), sq instanceof shapes().Shape, shapes().warn("loud"), shapes().warn("again"));
