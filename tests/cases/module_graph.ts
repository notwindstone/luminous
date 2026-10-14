// @native fromA fromB readLater shadow
// A cycle: `a` imports `b`, which imports `a` back, so `b` runs first: it
// calls `a`'s (hoisted) functions, and reading `a`'s `let` before `a` has
// run throws. A module's top-level names are renamed where another module's
// would clash, where a global another module uses has the name (`console`),
// or where a scope that refers to the import declares it; shorthand
// properties and destructuring keep their keys; anonymous default exports
// are named `default`; engine modules (`qjs:os`) are imported by each
// module that does.
import { fromA, aCount } from "./module_graph/a";
import Anon, { console as cconsole, x, y as why, p, q } from "./module_graph/c.ts";
import { x as x2 } from "./module_graph/c";
import anonFn from "./module_graph/d";
import triple from "./module_graph/e.js";
import { later } from "./module_graph/timers";
import { setTimeout } from "qjs:os";

function shadow(x$1: number): number {
  const x = 100;
  return x + x$1 + x2;
}
const obj = { x, why, p, q };
let total = 0;
({ total } = { total: x + 1 });
cconsole.log("via c", fromA(2), aCount);
console.log(shadow(1), JSON.stringify(obj), total, new Anon().hello(), Anon.name);
console.log(anonFn(), anonFn.name, triple(4), triple.name);
later(() => console.log("timers timer"));
setTimeout(() => console.log("main timer"), 0);
