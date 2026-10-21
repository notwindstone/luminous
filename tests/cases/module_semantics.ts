// @native readAll
// ESM semantics in a merged program: assigning to an import binding (or a
// namespace import, or `ns.x`) throws a TypeError and changes nothing;
// `delete ns.x` is false; a namespace object reports its exports as
// non-configurable writable data properties, has no prototype and cannot
// be extended; calling `ns.f()` passes the namespace as `this`; anonymous
// default exports are named `default` (a class with a static `name` keeps
// it); a JSON module (`with { type: "json" }`) is its parsed content, the
// same object for each import; an import whose binding is unused still runs
// its module.
import * as live from "./module_semantics/live";
import { count, bump, fixed } from "./module_semantics/live";
import Made from "./module_semantics/live";
import anon from "./module_semantics/named";
import double from "./module_semantics/arrow";
import Static from "./module_semantics/static_name";
import config from "./module_semantics/config.json" with { type: "json" };
import config2 from "./module_semantics/config.json" with { type: "json" };
import { unused } from "./module_semantics/side";

function attempt(f: () => unknown): string {
  try {
    f();
    return "ok";
  } catch (e) {
    return (e as Error).constructor.name;
  }
}

function readAll(): string {
  return [count, live.count, fixed].join("/");
}

const ns = live as unknown as Record<string, unknown>;
console.log(readAll(), bump(), readAll(), live.bump(), readAll());
console.log(
  attempt(() => ((count as number) = 5)),
  attempt(() => ((ns as { count: number }).count = 5)),
  attempt(() => (live as unknown as { fixed: number }).fixed++),
  Reflect.set(ns, "count", 9),
  Reflect.deleteProperty(ns, "count"),
  readAll(),
);
const d = Object.getOwnPropertyDescriptor(ns, "count")!;
console.log(d.value, d.writable, d.enumerable, d.configurable, Object.getPrototypeOf(ns), Object.isExtensible(ns));
console.log(Object.keys(ns).join(","), ns[Symbol.toStringTag], Reflect.defineProperty(ns, "count", { value: 2 }));
console.log(live.self() === ns, live.plain(), Made.make(), Made.name, anon(), anon.name, double(4), double.name);
console.log(live.bound(), live.picked(), live.later());
live.relink();
console.log(live.later() === ns);
console.log(typeof Static.name === "function" ? Static.name() : Static.name);
console.log(config.name, config.list.length, config.nested.ok, config === config2);
console.log((globalThis as unknown as { sideEffects: string[] }).sideEffects.join(","), typeof unused);
