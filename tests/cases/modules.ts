// @native sum dot add len bump makeLabel area
// A program of several modules, merged into one: an imported class's
// methods and an imported function are compiled with the importer (calls
// across modules are direct); imported `let` bindings are live; the
// modules' code runs dependencies first; names two modules both declare
// are kept apart; a default export, a namespace import (as `ns.x`, a type
// `ns.T`, and as a value: a frozen object of getters), re-exports (`export
// *`, `export { a as b } from`, `export default` through another module)
// and `.js` specifiers of `.ts` files resolve as in ESM; a module imported
// for its types only does not run.
import { Vec, dot, ORIGIN } from "./modules/vec";
import makeLabel, { counter, bump } from "./modules/counter";
import * as util from "./modules/lib/util.js";
import answer from "./modules/lib/util";
import type { Shape, Num } from "./modules/types";

function helper(): string {
  return "main-helper";
}

class Square implements Shape {
  constructor(public side: Num) {}
  area(): number {
    return this.side * this.side;
  }
}

function sum(n: number): number {
  let v = ORIGIN;
  for (let i = 0; i < n; i++) v = v.add(new Vec(i, 1));
  return v.len() + dot(v, new Vec(1, 1));
}

console.log("main start", helper(), makeLabel("x"));
console.log(counter, bump(3), counter, util.increment(4), counter);
console.log(sum(10).toFixed(3), new Square(3).area(), util.VERSION, answer, util.default);
const p: util.Pair = { a: 1, b: 2 };
const ns = util;
console.log(p.a + p.b, typeof ns, Object.keys(ns).join(","), ns.dot(new util.Vec(1, 2), new Vec(3, 4)));
console.log(Object.isFrozen(ns), Object.getPrototypeOf(ns), ns[Symbol.toStringTag], ns.increment === bump);
