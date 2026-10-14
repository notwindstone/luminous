// @native mk main typedNumbers fractions
// Native comparators for `sort`: the engine calls the compiled arrow function
// directly (objects and numbers, NaN and undefined, stability, exceptions,
// elements of other types).

type E = { id: number; salary: number; name: string };
function mk(n: number): E[] {
  const out: E[] = [];
  let seed = 7;
  for (let i = 0; i < n; i++) {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    out.push({ id: i, salary: seed % 1000, name: "e" + (seed % 37) });
  }
  return out;
}
function main(): string {
  const es = mk(500);
  const a = es.slice().sort((x, y) => y.salary - x.salary || x.id - y.id);
  const b = es.slice().sort((x, y) => (x.name < y.name ? -1 : x.name > y.name ? 1 : 0));
  const nums = [5, 3, 9, 1, 1, 7, 2.5, -1];
  const n1 = nums.slice().sort((p: number, q: number) => p - q);
  const mixed = [3, "x", 1] as unknown as number[];
  const n2 = mixed.slice().sort((p: number, q: number) => p - q);
  const withUndef = [3, undefined, 1, undefined, 2] as unknown as number[];
  const u = withUndef.slice().sort((p, q) => p - q);
  const stable = es.slice(0, 50).sort((x, y) => (x.salary % 3) - (y.salary % 3));
  let err = "";
  try {
    es.slice().sort((x, y) => {
      if (x.id === 3) throw new Error("cmp");
      return x.id - y.id;
    });
  } catch (e) {
    err = (e as Error).message;
  }
  const same = es.slice(0, 5);
  const r = same.sort((x, y) => x.id - y.id) === same;
  const bad = [1, 2, 3].sort(() => NaN);
  return [
    a.slice(0, 5).map((e) => e.id).join(","),
    b.slice(0, 5).map((e) => e.name + e.id).join(","),
    n1.join(","),
    String(n2),
    String(u),
    stable.map((e) => e.id).join(","),
    err,
    r,
    bad.join(","),
    JSON.stringify([].sort((x, y) => x - y)),
  ].join(" | ");
}

function typedNumbers(): string {
  const xs: number[] = [];
  for (let i = 0; i < 200; i++) xs.push(((i * 7919) % 211) - 100 + (i % 3) * 0.5);
  const asc = xs.slice().sort((a: number, b: number) => a - b);
  const desc = xs.slice().sort((a: number, b: number) => (a < b ? 1 : a > b ? -1 : 0));
  let ok = true;
  for (let i = 1; i < asc.length; i++) if (asc[i - 1] > asc[i]) ok = false;
  return [ok, asc.slice(0, 4).join(","), desc.slice(0, 4).join(","), asc.length].join(" ");
}

// Results that are not int32s: fractions on either side of 0, infinities,
// -0, NaN (for equal keys: as 0), and strings converted to numbers.
function fractions(): string {
  const es = mk(40);
  const a = es.slice().sort((x, y) => (x.salary - y.salary) / 1000);
  const b = es.slice().sort((x, y) => (x.salary === y.salary ? -0 : x.salary < y.salary ? -Infinity : Infinity));
  const key = (e: E) => (e.id % 7 === 3 ? 0 : e.salary + 1);
  const c = es.slice().sort((x, y) => (key(x) === key(y) ? NaN : (key(y) - key(x)) * 0.5));
  const d = es.slice().sort((x, y) => (String(x.salary - y.salary) as unknown as number));
  const ids = (xs: E[]) => xs.map((e) => e.id).join(",");
  return [ids(a), ids(b), ids(c), ids(d)].join(" | ");
}

console.log(main());
console.log(typedNumbers());
console.log(fractions());
