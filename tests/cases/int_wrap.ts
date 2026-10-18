// @native counts compound params loops captured bumpTotal assigns sumTo defaulted
// @interpreted dynamic
type int = number;

let total: int = 2147483640;

function counts(): string {
  let c: int = 2147483645;
  const olds: number[] = [];
  for (let k = 0; k < 4; k++) olds.push(c++);
  const pre = ++c;
  let d: int = -2147483646;
  const dold = d--;
  d--;
  const dpre = --d;
  return [olds.join(" "), c, pre, dold, d, dpre].join(",");
}

function compound(x: int, y: int): string {
  let a: int = x;
  a += y;
  let b: int = x;
  b -= y;
  let m: int = x;
  m *= y;
  let f: int = 7;
  f += 0.75 | 0;
  let big: int = 3;
  big *= 1410065408;
  big = big * big + (x - y) * 3;
  let s: int = 1;
  const v = (s += 2147483647);
  return [a, b, m, f, big, s, v].join(",");
}

function params(n: int, k: int): string {
  n++;
  n += k;
  k--;
  return n + "," + k;
}

function loops(n: int): string {
  let sum: int = 0;
  for (let i: int = 0; i < n; i++) sum += 1000000000;
  let laps: int = 0;
  for (let j: int = 2147483640; j > 0; j++) laps++;
  return sum + "," + laps;
}

function captured(): string {
  let c: int = 2147483647;
  const bump = () => {
    c++;
    c += 10;
  };
  bump();
  return String(c);
}

function bumpTotal(): number {
  total++;
  total += 5;
  return total;
}

function assigns(x: int, y: int): string {
  let a: int = x + y;
  let b: int = 0;
  b = b + x * y;
  let c: int = -x;
  c = c - 1;
  total = total + x;
  const twice = (n: int): int => n + n;
  const neg0: int = -0;
  return [a, b, c, total, twice(x), Object.is(neg0, -0), twice(y)].join(",");
}

function sumTo(n: int): int {
  let s: int = 0;
  for (let i: int = 1; i <= n; i++) s = s + i * i * i;
  return s * 2 + 1;
}

function defaulted(a: int, b: int = a + 2147483647): number {
  return b;
}

function dynamic(...xs: number[]): number {
  let c: int = 2147483647;
  for (const x of xs) c += x | 0;
  c++;
  return (new.target === undefined ? xs.length : -1) + c;
}

console.log(counts());
console.log(compound(2147483647, 1), compound(-2147483648, 1), compound(65537, 65537), compound(-5, 7));
console.log(params(2147483647, -2147483648), params(1, 2));
console.log(loops(5));
console.log(captured());
console.log(bumpTotal(), total);
total++;
console.log(total, bumpTotal());
console.log(dynamic(1, 2), dynamic());
console.log(assigns(2147483647, 2), assigns(-2147483648, -1), assigns(46341, 46341));
console.log(sumTo(10), sumTo(1000), sumTo(100000));
console.log(defaulted(5), defaulted(5, 6));
// Through the interpreted fallback (an argument that is not an int32).
const loose = params as unknown as (n: unknown, k: unknown) => string;
console.log(loose(1.5, 2), loose(2147483647, 0.5));
