// @native filterMap revenue count doubled logged mutating throwing withArray withIndex fields sorted
// Chains of `filter` and `map` (ending with an array, `.length` or
// `.reduce`) whose callbacks run no code are one loop (see src/chains.rs):
// what they give, and what runs, is still what the chain as written gives
// and runs: with getters, proxies, strings and other values where numbers
// were, holes, replaced builtins, a subclass, empty arrays, -0 and NaN; and
// callbacks that log, mutate, throw or use the array are not merged.
type Item = { id: number; price: number; qty: number; tag: number };

function filterMap(xs: Item[]): number[] {
  return xs.filter((x) => x.tag !== 0).map((x) => x.price * x.qty);
}
function revenue(xs: Item[]): number {
  return xs.map((x) => x.price * x.qty).reduce((acc, v) => acc + v, 0);
}
function count(xs: Item[]): number {
  return xs.filter((x) => x.price > 50).map((x) => x.id).length;
}
function doubled(ns: number[]): number[] {
  return ns.filter((n) => n > 2).map((n) => n * 2);
}
const log: string[] = [];
function logged(xs: Item[]): number[] {
  return xs
    .filter((x) => {
      log.push("f" + x.id);
      return x.tag !== 0;
    })
    .map((x) => {
      log.push("m" + x.id);
      return x.price;
    });
}
function mutating(xs: Item[]): number[] {
  return xs
    .filter((x) => {
      x.qty = x.qty + 1;
      return x.qty > 2;
    })
    .map((x) => x.qty);
}
function throwing(xs: Item[]): number[] {
  return xs
    .filter((x) => {
      if (x.price < 0) throw new RangeError("negative " + x.id);
      return true;
    })
    .map((x) => x.price * 2);
}
function withArray(xs: Item[]): number[] {
  return xs.filter((x) => x.tag !== 0).map((x, i, a) => x.price + a.length + i);
}
function withIndex(xs: Item[]): number[] {
  return xs.filter((x) => x.tag !== 0).map((x, i) => x.price + i);
}
function fields(xs: Item[]): number[] {
  return xs.filter((x) => x.qty > 0).map((x) => x.price);
}
function sorted(xs: Item[]): number[] {
  return xs
    .filter((x) => x.qty > 3)
    .slice()
    .sort((a, b) => b.price - a.price || a.id - b.id)
    .map((x) => x.price);
}

const items: Item[] = [];
for (let i = 0; i < 12; i++) items.push({ id: i, price: (i * 37) % 101, qty: (i * 13) % 17, tag: i % 5 });
console.log("plain", filterMap(items).join(","), revenue(items), count(items));
console.log("numbers", doubled([1, 2, 3, 4, 5]).join(","), doubled([]).length, revenue([]));
console.log("sorted", sorted(items).join(","));

// A getter on an element: each read when the chain as written reads it.
const reads: string[] = [];
const withGetter = items.slice(0, 4);
withGetter[2] = {
  id: 2,
  get price() {
    reads.push("price");
    return 5;
  },
  qty: 2,
  tag: 1,
} as any;
console.log("getter", filterMap(withGetter).join(","), revenue(withGetter), reads.join(","));

// A proxy element.
const traps: string[] = [];
const proxied = items.slice(0, 3);
proxied[1] = new Proxy(
  { id: 1, price: 10, qty: 3, tag: 1 },
  {
    get(t: any, k: string) {
      traps.push(k);
      return t[k];
    },
  },
);
console.log("proxy", filterMap(proxied).join(","), revenue(proxied), traps.join(","));

// Strings, objects with valueOf, -0 and NaN where numbers were.
const odd: any[] = [
  { id: 0, price: "3", qty: 2, tag: 1 },
  { id: 1, price: 4, qty: { valueOf: () => 5 }, tag: 1 },
  { id: 2, price: -0, qty: 1, tag: 1 },
  { id: 3, price: NaN, qty: 1, tag: "x" },
];
console.log("odd", filterMap(odd).map((v) => Object.is(v, -0) ? "-0" : String(v)).join(","), revenue(odd), count(odd));
console.log("strings", revenue([{ id: 0, price: 1, qty: 1, tag: 1 }, { id: 1, price: "a", qty: 1, tag: 1 }] as any));

// Holes, a subclass, an array with own properties.
const holes: Item[] = items.slice(0, 4);
delete (holes as any)[1];
console.log("holes", filterMap(holes).join(","), revenue(holes), count(holes));
class Items extends Array<Item> {}
const sub = Items.from(items.slice(0, 5));
const subResult = filterMap(sub);
console.log("subclass", subResult.join(","), subResult instanceof Items, revenue(sub));
const own: any = items.slice(0, 5);
own.extra = 1;
console.log("own", filterMap(own).join(","), revenue(own));

// The callbacks that log, mutate, throw, or use the array or the index.
console.log("logged", logged(items.slice(0, 4)).join(","), log.join(","));
const copies = items.slice(0, 5).map((x) => ({ ...x }));
console.log("mutating", mutating(copies).join(","), copies.map((x) => x.qty).join(","));
try {
  throwing([{ id: 0, price: 1, qty: 1, tag: 1 }, { id: 1, price: -1, qty: 1, tag: 1 }]);
} catch (e) {
  console.log("throwing", (e as Error).message);
}
console.log("withArray", withArray(items.slice(0, 5)).join(","), "withIndex", withIndex(items.slice(0, 5)).join(","));

// Replaced builtins: map, then reduce, then the species.
const originalMap = Array.prototype.map;
(Array.prototype as any).map = function (this: any[], f: any) {
  log.push("map!");
  return originalMap.call(this, f);
};
console.log("replaced map", filterMap(items.slice(0, 3)).join(","), log[log.length - 1]);
Array.prototype.map = originalMap;
const originalReduce = Array.prototype.reduce;
(Array.prototype as any).reduce = function () {
  return -1;
};
console.log("replaced reduce", revenue(items));
Array.prototype.reduce = originalReduce;
console.log("restored", revenue(items), fields(items).length);
Object.defineProperty(Array, Symbol.species, {
  get() {
    log.push("species");
    return Array;
  },
  configurable: true,
});
console.log("species", filterMap(items.slice(0, 3)).join(","), log.filter((s) => s === "species").length);
