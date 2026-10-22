// Array method chains over arrays of records: filter then map, filter,
// slice, sort and map, and map then reduce. The callbacks are pure.
type Item = { id: number; price: number; qty: number; tag: number };

const items: Item[] = [];
for (let i = 0; i < 2000; i++) items.push({ id: i, price: (i * 37) % 101, qty: (i * 13) % 17, tag: i % 5 });

function filterMap(xs: Item[]): number {
  const totals = xs.filter((x) => x.tag !== 0).map((x) => x.price * x.qty);
  return totals.length + totals[totals.length - 1];
}

function topPrices(xs: Item[]): number {
  const top = xs
    .filter((x) => x.qty > 3)
    .slice()
    .sort((a, b) => b.price - a.price || a.id - b.id)
    .map((x) => x.price);
  return top[0] + top[top.length - 1] + top.length;
}

function revenue(xs: Item[]): number {
  return xs.map((x) => x.price * x.qty).reduce((acc, v) => acc + v, 0);
}

function count(xs: Item[]): number {
  return xs.filter((x) => x.price > 50).map((x) => x.id).length;
}

const start = Date.now();
let a = 0, b = 0, c = 0, d = 0;
for (let r = 0; r < 2000; r++) {
  a += filterMap(items);
  c += revenue(items);
  d += count(items);
}
const mid = Date.now();
for (let r = 0; r < 300; r++) b += topPrices(items);
const end = Date.now();
console.log(`filterMap: ${a}`);
console.log(`topPrices: ${b}`);
console.log(`revenue: ${c}`);
console.log(`count: ${d}`);
console.log(`elapsed chains: ${mid - start} ms`);
console.log(`elapsed sort: ${end - mid} ms`);
console.log(`elapsed: ${end - start} ms`);
