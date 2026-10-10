// @native loops labeled logic counter dowhile seq truthy boolRet voidFn early
function loops(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    if (i % 3 === 0) continue;
    if (i > 50) break;
    s += i;
  }
  let j = n;
  while (j > 0) j -= 7;
  for (;;) {
    s += 1;
    if (s > 10000) break;
    if (s % 2 === 0) { s *= 3; continue; }
    s = s + 0.5 | 0;
    break;
  }
  return s + j;
}
function labeled(n: number): number {
  let count = 0;
  outer: for (let i = 0; i < n; i++) {
    inner: for (let j = 0; j < n; j++) {
      if (j > i) continue outer;
      if (i + j > n) break outer;
      if (j === 3) continue inner;
      count += i * j;
    }
  }
  return count;
}
function logic(a: number, b: number): number {
  let x = 0;
  const r1 = a && (x = 5);
  const r2 = b || (x += 10);
  const r3 = (a > 0 && b > 0) || a === b;
  return r1 * 100 + r2 * 10 + x + (r3 ? 1000 : 2000) + (!a ? 1 : 0);
}
function counter(n: number): number {
  let i = n;
  let a = i++;
  let b = ++i;
  let c = i--;
  let d = --i;
  i += 2; i -= 1; i *= 3; i /= 2; i %= 5; i **= 2;
  let k = 7;
  k <<= 3; k >>= 1; k >>>= 1; k &= 0xff; k |= 0x100; k ^= 0x55;
  return a * 1e6 + b * 1e4 + c * 100 + d + i * 1e-3 + k * 1e-6;
}
function dowhile(n: number): number {
  let i = 0;
  let s = 0;
  do {
    i++;
    if (i === 2) continue;
    s += i;
  } while (i < n);
  return s;
}
function seq(a: number): number {
  let x = 1;
  return (x += a, x *= 2, x - 1);
}
function truthy(x: number): boolean {
  if (x) return true;
  return false;
}
function boolRet(a: boolean, b: boolean): boolean {
  return a === b ? !a : a && !b;
}
function voidFn(a: number): void {
  let x = a;
  if (x > 0) return;
  x = 1;
}
function early(x: number): number {
  while (true) {
    if (x > 100) return x;
    x = x * 2 + 1;
  }
}

for (const n of [0, 1, 5, 10, 100]) console.log(loops(n), labeled(n), dowhile(n), seq(n), early(n));
for (const a of [0, 1, -1, NaN, 2]) for (const b of [0, 1, NaN, 2]) console.log(logic(a, b));
console.log(counter(3), counter(-2.5), counter(1e9));
console.log([0, -0, 1, NaN, -1, Infinity, 1e-300].map(truthy).join(" "));
console.log(boolRet(true, true), boolRet(true, false), boolRet(false, true), boolRet(false, false));
console.log(voidFn(1), voidFn(-1));
