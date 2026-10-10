// @native sum build mixed describeAll at swap
aot function sum(xs: number[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += xs[i];
  return s;
}
aot function build(n: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < n; i++) out.push(i * i);
  return out;
}
type Item = { hi: "wow" } | string | number;
aot function mixed(): Array<Item> {
  return [{ hi: "wow" }, "str", 42, 1.5];
}
aot function describeAll(items: Array<Item>): string {
  let out = "";
  for (let i = 0; i < items.length; i++) {
    const it = items[i];
    if (typeof it === "string") out += "s:" + it + " ";
    else if (typeof it === "number") out += "n:" + it + " ";
    else out += "o:" + it.hi + " ";
  }
  return out;
}
aot function at(xs: unknown[], i: number): unknown {
  return xs[i];
}
aot function swap(xs: unknown[], i: number, j: number): void {
  const t = xs[i];
  xs[i] = xs[j];
  xs[j] = t;
}

console.log(sum([1, 2, 3.5]), sum([]), sum(["1", 2] as never), JSON.stringify(build(5)));
console.log(JSON.stringify(mixed()), describeAll(mixed()));
console.log(at([1, 2], 1), at([1, 2], 5), at([1, 2], -1), at([1, 2], 0.5));
const xs = [1, "two", { three: 3 }];
swap(xs, 0, 2);
console.log(JSON.stringify(xs));
