// @native m1 m2 minmax pred consts
function m1(x: number, k: number): number {
  if (k === 0) return Math.abs(x);
  if (k === 1) return Math.sqrt(x);
  if (k === 2) return Math.floor(x);
  if (k === 3) return Math.ceil(x);
  if (k === 4) return Math.trunc(x);
  if (k === 5) return Math.round(x);
  if (k === 6) return Math.fround(x);
  if (k === 7) return Math.sign(x);
  if (k === 8) return Math.clz32(x);
  if (k === 9) return Math.sin(x);
  if (k === 10) return Math.cos(x);
  if (k === 11) return Math.tan(x);
  if (k === 12) return Math.exp(x);
  if (k === 13) return Math.log(x);
  if (k === 14) return Math.log2(x);
  if (k === 15) return Math.log10(x);
  if (k === 16) return Math.cbrt(x);
  if (k === 17) return Math.atan(x);
  if (k === 18) return Math.expm1(x);
  if (k === 19) return Math.log1p(x);
  if (k === 20) return Math.tanh(x);
  if (k === 21) return Math.asinh(x);
  return Math.acosh(x);
}
function m2(x: number, y: number, k: number): number {
  if (k === 0) return Math.pow(x, y);
  if (k === 1) return Math.atan2(x, y);
  if (k === 2) return Math.imul(x, y);
  if (k === 3) return Math.hypot(x, y);
  if (k === 4) return Math.min(x, y);
  return Math.max(x, y);
}
function minmax(a: number, b: number, c: number): number {
  return Math.min(a, b, c) * 1000 + Math.max(a, b, c) + Math.min() + Math.max() + Math.min(a) - a;
}
function pred(x: number): number {
  let r = 0;
  if (Number.isNaN(x)) r += 1;
  if (Number.isFinite(x)) r += 2;
  if (Number.isInteger(x)) r += 4;
  if (Number.isSafeInteger(x)) r += 8;
  return r;
}
function consts(): number {
  return Math.PI + Math.E + Math.LN2 + Math.LN10 + Math.LOG2E + Math.LOG10E + Math.SQRT2 + Math.SQRT1_2 +
    Number.EPSILON + Number.MIN_VALUE;
}

const show = (x: number) => (Object.is(x, -0) ? "-0" : String(x));
const vals = [0, -0, 0.5, -0.5, 0.49999999999999994, -0.49999999999999994, 1.5, 2.5, -2.5, -1.5, 3.7, -3.7,
  1, -1, 2 ** 31, -(2 ** 31), 2 ** 32 + 1, 4503599627370495.5, 2 ** 53, 2 ** 53 + 2, 1e300, Number.MIN_VALUE,
  Infinity, -Infinity, NaN];
for (let k = 0; k <= 22; k++) console.log("m1", k, vals.map((x) => show(m1(x, k))).join(" "));
for (let k = 0; k <= 5; k++) {
  const out: string[] = [];
  for (const x of vals) for (const y of vals) out.push(show(m2(x, y, k)));
  console.log("m2", k, out.join(" "));
}
console.log(show(minmax(1, 2, 3)), show(minmax(0, -0, 0)), show(minmax(NaN, 1, 2)), show(minmax(-0, 0, -0)));
console.log(vals.map(pred).join(" "));
console.log(consts());
