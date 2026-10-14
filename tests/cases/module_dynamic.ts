// Dynamic import(): modules of the program that only import() imports are
// evaluated lazily (in a later job, dependencies first, once), and import()
// returns their namespace objects; import.meta.
import { value as mainValue } from "./module_dynamic/eager.ts";

console.log("main start");
const pending = import("./module_dynamic/lazy.ts");
console.log("after the import() call");
const lazy = await pending;
console.log("lazy:", lazy.value, lazy.twice(4), lazy.default, lazy.named(), mainValue);
console.log("keys:", Object.keys(lazy).join(","));
console.log("same namespace:", lazy === (await import("./module_dynamic/lazy.ts")));
lazy.bump();
console.log("live binding:", lazy.counter);

// A module that awaits: evaluated once for concurrent imports.
const [s1, s2] = await Promise.all([import("./module_dynamic/slow.ts"), import("./module_dynamic/slow.ts")]);
console.log("slow:", s1 === s2, s1.ready);

// A module that throws: every import rejects with its error.
for (let i = 0; i < 2; i++) {
  try {
    await import("./module_dynamic/fails.ts");
  } catch (e) {
    console.log("fails:", (e as Error).message);
  }
}
for (let i = 0; i < 2; i++) {
  try {
    await import("./module_dynamic/fails_async.ts");
  } catch (e) {
    console.log("fails_async:", (e as Error).message);
  }
}

// Cycles among lazily evaluated modules.
const cycle = await import("./module_dynamic/cycle_a.ts");
console.log("cycle:", cycle.a(), cycle.fromB());

// Top-level code of every kind.
const top = await import("./module_dynamic/toplevel.ts");
console.log("toplevel:", top.summary());

// Default exports.
const f = (await import("./module_dynamic/def_fn.ts")).default;
const C = (await import("./module_dynamic/def_class.ts")).default;
const g = (await import("./module_dynamic/def_arrow.ts")).default;
console.log("defaults:", f(), f.name, new C().name(), C.name, g(), g.name);

// A module of the program that already ran.
const eager = await import("./module_dynamic/eager.ts");
console.log("eager:", eager.value === mainValue);

// JSON, and import() in a function called later.
async function load(): Promise<number> {
  const j = await import("./module_dynamic/data.json", { with: { type: "json" } });
  return j.default.items.length;
}
console.log("json:", await load(), await load());

// A computed specifier: the engine loads and interprets the module at run
// time (with a warning, once).
for (const which of ["plugin", "plugin"]) {
  const plugin = await import(`./module_dynamic/${which}.js`);
  console.log("plugin:", plugin.greet("luminous"));
}

// A module that does not link: import() rejects (the build warns).
try {
  await import("./module_dynamic/broken.ts");
} catch (e) {
  console.log("broken:", (e as Error).name);
}

console.log("meta:", typeof import.meta.url, import.meta.main, import.meta.url.endsWith("/module_dynamic.ts"));
console.log("main end");
