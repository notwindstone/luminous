// @native makeAdders named tweak many keep asyncs nested asyncArrows
// Closures native code makes inline after the first of each function: each
// its own object, with its own data, `length` and `name`, and changes to one
// (its properties, its prototype) not seen by the others.
function makeAdders(n: number): Array<(x: number) => number> {
  const out: Array<(x: number) => number> = [];
  for (let i = 0; i < n; i++) out.push((x: number) => x + i);
  return out;
}

function named(k: number) {
  const pick = (a: number, b: number) => a * k + b;
  return pick;
}

function tweak(): string {
  const fs = [named(1), named(2), named(3), named(4)];
  const res: string[] = [];
  // Its own properties: a new one, a redefined name, a deleted length.
  (fs[0] as any).extra = 42;
  Object.defineProperty(fs[1], "name", { value: "renamed" });
  delete (fs[2] as any).length;
  Object.setPrototypeOf(fs[3], { tag: "proto" });
  const more = [named(5), named(6)];
  for (const f of fs.concat(more)) {
    res.push(`${f.name}/${f.length}/${(f as any).extra}/${(f as any).tag}/${typeof f.call}/${f(10, 1)}`);
  }
  res.push(String(Object.getPrototypeOf(more[0]) === Function.prototype));
  res.push(JSON.stringify(Object.getOwnPropertyNames(more[1])));
  return res.join(" ");
}

// Enough closures, each holding others, for collections to run while they
// are made (and with the check engine: often).
function many(count: number): number {
  let keep: Array<() => number> = [];
  let total = 0;
  for (let i = 0; i < count; i++) {
    const s = "v" + i;
    const prev = keep.length > 0 ? keep[keep.length - 1] : () => 0;
    const f = () => s.length + prev.length + i;
    keep.push(f);
    if (keep.length > 64) keep = keep.slice(32);
    total += f();
  }
  return total;
}

function keep(): Array<() => string> {
  const fs: Array<() => string> = [];
  for (let i = 0; i < 2000; i++) {
    const obj = { i, s: "x".repeat(i % 7) };
    fs.push(() => obj.s + obj.i);
  }
  return fs;
}

async function asyncs(n: number): Promise<number> {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const f = async (x: number) => x * 2;
    sum += await f(i);
    if (Object.getPrototypeOf(f) === Function.prototype) sum = -1;
  }
  return sum;
}

function nested(n: number): number {
  let acc = 0;
  for (let i = 0; i < n; i++) {
    const outer = (a: number) => (b: number) => (c: number) => a + b + c + i;
    acc += outer(1)(2)(3);
  }
  return acc;
}

const adders = makeAdders(5);
console.log(adders.map((f) => f(100)).join(","), adders[0] !== adders[1], adders[3].name === "", adders[4].length);
console.log(tweak());
console.log(many(20000));
const kept = keep();
console.log(kept.length, kept[0](), kept[1999](), kept.map((f) => f()).join("").length);
console.log(nested(1000));
asyncs(50).then((v) => console.log("asyncs", v));

// Async arrows: of AsyncFunction.prototype from the start (one shape for
// all, made inline too), their prototype still their own to change.
async function asyncArrows(n: number): Promise<string> {
  const fs: Array<(x: number) => Promise<number>> = [];
  for (let i = 0; i < n; i++) fs.push(async (x: number) => x + i);
  Object.setPrototypeOf(fs[1], Function.prototype);
  (fs[2] as any).own = "own";
  const AsyncFunction = Object.getPrototypeOf(async () => {}).constructor;
  let sum = 0;
  for (const f of fs) sum += await f(1);
  return [sum, fs[0] instanceof AsyncFunction, fs[1] instanceof AsyncFunction, fs[3] instanceof AsyncFunction,
    (fs[2] as any).own, (fs[3] as any).own, fs[3].name, fs[3].length, Object.prototype.toString.call(fs[4])].join(" ");
}
asyncArrows(6).then((v) => console.log("asyncArrows", v));
