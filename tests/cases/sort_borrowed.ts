// @native byKey keepArgs reassign throwing strings
// Native `sort` comparators borrow the elements the sort holds: one that
// keeps its arguments elsewhere, one that reassigns them, one that
// throws midway, and strings, leave every element alive exactly once.

type Item = { k: number; name: string };

const kept: Item[] = [];

function items(n: number): Item[] {
  const out: Item[] = [];
  for (let i = 0; i < n; i++) out.push({ k: (i * 7919) % n, name: "i" + i });
  return out;
}

function byKey(xs: Item[]): string {
  const sorted = xs.slice().sort((a, b) => a.k - b.k || (a.name < b.name ? -1 : 1));
  return sorted.map((x) => x.k).join(",");
}

function keepArgs(xs: Item[]): string {
  xs.sort((a, b) => {
    if (kept.length < 5) kept.push(a, b);
    return b.k - a.k;
  });
  return xs.map((x) => x.name).join(" ") + " | " + kept.length + " " + kept.every((x) => xs.includes(x));
}

function reassign(xs: Item[]): string {
  xs.sort((a, b) => {
    const x = a;
    a = b;
    b = x;
    return a.k - b.k;
  });
  return xs.map((x) => x.k).join(",");
}

function throwing(xs: Item[]): string {
  let calls = 0;
  try {
    xs.sort((a, b) => {
      if (a.k === 3 || b.k === 3) throw new RangeError("three");
      return a.k - b.k;
    });
  } catch (e) {
    calls++;
    return (e as Error).message + " " + calls + " " + xs.length + " " + xs.map((x) => x.k).sort((p, q) => p - q).join(",");
  }
  return "no throw";
}

function strings(words: string[]): string {
  return words.slice().sort((a, b) => (a.length - b.length) || (a < b ? -1 : a > b ? 1 : 0)).join(" ");
}

console.log(byKey(items(12)));
console.log(keepArgs(items(6)));
console.log(reassign(items(9)));
console.log(throwing(items(8)));
console.log(strings(["pear", "fig", "apple", "kiwi", "banana", "date", "fig"]));
