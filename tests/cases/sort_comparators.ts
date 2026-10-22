// @native byPrice counting inconsistent throwing coerced numbers
// `sort` with a comparator calls it with the same values in the same order,
// natively compiled or not (the engine sorts as native code does, see
// patch 0040): a comparator that counts and logs, one that is not a
// consistent order (whose result then depends on the algorithm), one that
// throws midway, one that returns objects (converted with valueOf).
type Item = { id: number; price: number };
const calls: string[] = [];

function byPrice(xs: Item[]): number[] {
  return xs
    .slice()
    .sort((a, b) => a.price - b.price || a.id - b.id)
    .map((x) => x.id);
}
let count = 0;
function counting(xs: Item[]): number[] {
  return xs
    .slice()
    .sort((a, b) => {
      count++;
      calls.push(a.id + ":" + b.id);
      return a.price - b.price;
    })
    .map((x) => x.id);
}
function inconsistent(xs: Item[]): number[] {
  return xs
    .slice()
    .sort((a, b) => (a.id % 3) - (b.price % 4))
    .map((x) => x.id);
}
function throwing(xs: Item[]): string {
  const c = xs.slice();
  try {
    c.sort((a, b) => {
      calls.push("t" + a.id);
      if (calls.length > 30) throw new Error("stop " + a.id + " " + b.id);
      return b.price - a.price;
    });
  } catch (e) {
    return (e as Error).message + " " + c.map((x) => x.id).join(",");
  }
  return "no throw";
}
function coerced(xs: Item[]): number[] {
  return xs
    .slice()
    .sort((a, b) => ({ valueOf: () => (calls.push("v"), a.price - b.price) }) as any)
    .map((x) => x.id);
}
function numbers(ns: number[]): number[] {
  return ns.slice().sort((a, b) => {
    calls.push("n" + a);
    return a - b;
  });
}

const items: Item[] = [];
for (let i = 0; i < 23; i++) items.push({ id: i, price: (i * 37) % 11 });
console.log("byPrice", byPrice(items).join(","));
console.log("counting", counting(items).join(","), count, calls.slice(0, 12).join(" "));
console.log("inconsistent", inconsistent(items).join(","), inconsistent(items.slice(0, 7)).join(","));
calls.length = 0;
console.log("throwing", throwing(items));
calls.length = 0;
console.log("coerced", coerced(items.slice(0, 9)).join(","), calls.length);
calls.length = 0;
console.log("numbers", numbers([5, 3, 9, 1, 7, 2, 8, 6, 4, 0, 11, 10]).join(","), calls.join(""));
