// @native add3 limits countDown wrap products rems maths bits pick viaArrays isPrime count gen asyncSum captured
type int = number;

function add3(a: int, b: int, c: int): int {
  return (a + b + c) | 0;
}

function limits(): string {
  let out = "";
  for (let i: int = 2147483640; i < 2147483647; i++) out += i - 2147483640;
  for (let i: int = -2147483643; i > -2147483648; i--) out += "," + i;
  let last: int = 0;
  for (let i: int = 0; i <= 5; i++) last = i;
  return out + "|" + last;
}

function countDown(n: int): int {
  let steps: int = 0;
  for (let i: int = n; i > 0; i--) steps = (steps + 1) | 0;
  return steps;
}

function wrap(): string {
  const big: int = 2147483647;
  const small: int = -2147483648;
  return [(big + 1) | 0, (small - 1) | 0, big + 1, small - 1, -small, big * 2].join(",");
}

function products(a: int, b: int, c: int): string {
  const p = a * b;
  return [p, p > c, p < c, p === c, p <= 2147483647, a * b === 9007199254740993, Object.is(a * 0, -0), Object.is(-a * 0, -0)].join(",");
}

function rems(a: int, b: int): string {
  const z: int = 0;
  const r = a % b;
  return [
    r,
    Object.is(r, -0),
    a % z,
    a % z === 0,
    a % z !== 0,
    a % z < 1,
    (a % b) === 0,
    (a % b) !== 0,
    Object.is(-2147483648 % -1, -0),
  ].join(",");
}

function maths(a: int, b: int): string {
  return [Math.imul(a, b), Math.clz32(a), Math.min(a, b), Math.max(a, b, 7), Math.abs(a), -a].join(",");
}

function bits(x: int): string {
  let h: int = x;
  for (let i: int = 0; i < 10; i++) {
    h ^= h << 7;
    h = h ^ (h >> 3);
    h = (h + 0x9e37) | 0;
  }
  const u = h >>> 0;
  return [h, u, ~h, h & 0xff, u > 2147483647].join(",");
}

function pick(c: boolean, a: int, b: int): string {
  const x: int = c ? a : b;
  const y: int = a || b;
  return x + "," + y + "," + (c ? a : 0.5);
}

function viaArrays(xs: Float64Array, n: int): number {
  let s = 0;
  for (let i: int = 0; i < n; i++) s += xs[i] + xs[i + 1];
  return s;
}

function isPrime(n: int): boolean {
  if (n < 2) return false;
  if (n % 2 === 0) return n === 2;
  for (let d: int = 3; d * d <= n; d = (d + 2) | 0) {
    if (n % d === 0) return false;
  }
  return true;
}

function count(limit: int): int {
  let c: int = 0;
  for (let n: int = 0; n < limit; n++) if (isPrime(n)) c = (c + 1) | 0;
  return c;
}

function* gen(n: int): Generator<number> {
  for (let i: int = 0; i < n; i++) {
    const sq: int = Math.imul(i, i);
    yield sq;
  }
}

async function asyncSum(n: int): Promise<number> {
  let s: int = 0;
  for (let i: int = 0; i < n; i++) {
    await null;
    s = (s + i) | 0;
  }
  return s;
}

function captured(n: int): number {
  let k: int = n;
  const bump = () => {
    k = k + 0.5;
  };
  bump();
  return k;
}

console.log(add3(1, 2, 3), add3(2147483647, 1, 0), add3(-5, 2, 1));
console.log(limits());
console.log(countDown(5), countDown(-3));
console.log(wrap());
console.log(products(46341, 46341, 2147483647), products(-3, 0, 0), products(94906267, 94906267, 5));
console.log(rems(-4, 2), rems(7, -3), rems(-2147483648, -1));
console.log(maths(-7, 3), maths(65536, 65536));
console.log(bits(12345), bits(-1));
console.log(pick(true, 3, 4), pick(false, 0, 4));
console.log(viaArrays(new Float64Array([1, 2, 3, 4, 5]), 4));
console.log(count(10000));
console.log([...gen(5)].join(","));
console.log(captured(1));
// Arguments that are not int32s: the entry declines to the interpreted
// version, which has JS semantics.
const loose = add3 as unknown as (a: unknown, b: unknown, c: unknown) => unknown;
console.log(loose(1.5, 2, 3), loose("1", 2, 3), loose(-0, 0, 0), loose(2 ** 31, 0, 0), loose(1, 2, 3));
const looseDown = countDown as unknown as (n: unknown) => unknown;
console.log(looseDown(2.5), looseDown(NaN), looseDown(4.0));
asyncSum(5).then((s) => console.log("async", s));
