// @native leaf mid early anyRet withDefault throws counted recurse order sideEffects driver
// Small direct calls are inlined: results, early returns, exceptions,
// argument order and side effects must not change.

let log: string[] = [];
let counter = 0;

function leaf(x: number, salt: int): number {
  let y: int = ((x | 0) ^ salt) | 0;
  y = (y * 1664525 + 1013904223) | 0;
  return y;
}

function mid(x: number, salt: int): number {
  return leaf(x + 1, salt) * 0.5 + leaf(x - 1, (salt + 1) | 0);
}

function early(x: number): string {
  if (x < 0) return "neg";
  if (x === 0) {
    return "zero";
  }
  return "pos" + x;
}

function anyRet(x: number): unknown {
  if (x > 1) return { x };
  return x > 0 ? "one" : undefined;
}

function withDefault(a: number, b: number = 10): number {
  return a + b;
}

function throws(x: number): number {
  if (x === 3) throw new Error("three");
  return x * 2;
}

function counted(): number {
  counter++;
  return counter;
}

function recurse(n: number): number {
  return n <= 0 ? 0 : n + recurse(n - 1);
}

function order(a: number, b: number, c: number): string {
  return a + "," + b + "," + c;
}

function sideEffects(tag: string): number {
  log.push(tag);
  return tag.length;
}

function driver(): string {
  const out: unknown[] = [];
  let acc = 0;
  for (let i = 0; i < 20; i++) acc += mid(i, 7);
  out.push(acc);
  for (const x of [-1, 0, 5]) out.push(early(x));
  for (const x of [0, 1, 2]) out.push(JSON.stringify(anyRet(x)));
  out.push(withDefault(1), withDefault(1, 2));
  let caught = 0;
  for (let i = 0; i < 6; i++) {
    try {
      acc += throws(i);
    } catch (e) {
      caught++;
      out.push((e as Error).message);
    }
  }
  out.push(acc, caught);
  out.push(order(counted(), counted(), counted()), counter);
  out.push(recurse(10));
  out.push(sideEffects("a") + sideEffects("bb") * sideEffects("ccc"), log.join(""));
  // In a condition and nested in other calls.
  if (early(leaf(3, 1)) !== "neg") out.push("cond");
  out.push(withDefault(withDefault(1), mid(2, 3)));
  return out.join(";");
}

console.log(driver());
console.log(leaf(1e10, 5), leaf(-0.5, 0), leaf(NaN, 1), mid(2 ** 40, -3));
