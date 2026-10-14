// @native basics mutate holes arrayLike throwing reduceAll
// Array callbacks (map, filter, forEach, some, every, reduce,
// reduceRight) entered directly from the builtins: mutation during the
// loop, holes, array-likes, thisArg, exceptions, non-native callbacks.

function basics(n: number): string {
  const xs: number[] = [];
  for (let i = 0; i < n; i++) xs.push((i * 7) % 11);
  const m = xs.map((x, i) => x * 2 + i);
  const f = xs.filter((x) => x > 5);
  let s = 0;
  xs.forEach((x, i, arr) => {
    s += x * i + arr.length;
  });
  return [
    m.join(","),
    f.join(","),
    s,
    xs.some((x) => x === 10),
    xs.every((x) => x < 11),
    xs.every((x) => x < 10),
    xs.reduce((a, b) => a + b, 0),
    xs.reduceRight((a, b) => a + "" + b, ""),
  ].join(" ");
}

function mutate(): string {
  const xs = [1, 2, 3, 4, 5, 6];
  const seen: number[] = [];
  const m = xs.map((x, i) => {
    seen.push(x);
    if (i === 1) xs.pop();
    if (i === 2) xs.push(99);
    return x * 10;
  });
  const ys = [1, 2, 3];
  const r = ys.reduce((a, b, i) => {
    if (i === 1) ys.length = 2;
    return a + b;
  });
  return [JSON.stringify(m), seen.join(","), xs.join(","), r].join(" ");
}

function holes(): string {
  const xs = [1, , 3, , 5];
  const ys: number[] = new Array(4);
  ys[2] = 7;
  return [
    JSON.stringify(xs.map((x) => (x as number) * 2)),
    JSON.stringify(xs.filter(() => true)),
    xs.reduce((a, b) => (a as number) + (b as number)),
    JSON.stringify(ys.map((x) => x + 1)),
    1 in xs.map((x) => x),
  ].join(" ");
}

function arrayLike(): string {
  const o = { length: 3, 0: "a", 1: "b", 2: "c" } as unknown as string[];
  const out = Array.prototype.map.call(o, (x: string, i: number) => x + i) as string[];
  return out.join(",") + " " + Array.prototype.reduce.call(o, (a: string, b: string) => a + b, ">");
}

function thisArgs(): string {
  const ctx = { k: 3 };
  const xs = [1, 2, 3];
  const r = xs.map(function (this: { k: number }, x: number) {
    return x * this.k;
  }, ctx);
  return r.join(",");
}

function throwing(): string {
  const xs = [1, 2, 3, 4];
  let calls = 0;
  try {
    xs.forEach((x) => {
      calls++;
      if (x === 3) throw new Error("stop at " + x);
    });
  } catch (e) {
    return (e as Error).message + " after " + calls;
  }
  return "no throw";
}

function reduceAll(): string {
  const xs = [3, 1, 4, 1, 5, 9, 2, 6];
  const max = xs.reduce((a, b) => Math.max(a, b));
  const pairs = xs.reduce((acc, x, i) => (i % 2 ? acc : acc.concat([[x, xs[i + 1]]])), [] as number[][]);
  let err = "";
  try {
    ([] as number[]).reduce((a, b) => a + b);
  } catch (e) {
    err = (e as Error).constructor.name;
  }
  return [max, JSON.stringify(pairs), err, [5].reduce((a) => a * 100)].join(" ");
}

console.log(basics(30));
console.log(mutate());
console.log(holes());
console.log(arrayLike());
console.log(thisArgs());
console.log(throwing());
console.log(reduceAll());
console.log([1, 2, 3].map(String).join("|"), ["1", "2", "3"].map(Number).reduce((a, b) => a + b));
