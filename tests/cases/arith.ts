// @native add sub mul div rem pow neg band bor bxor shl sar shr bnot
function add(a: number, b: number): number { return a + b; }
function sub(a: number, b: number): number { return a - b; }
function mul(a: number, b: number): number { return a * b; }
function div(a: number, b: number): number { return a / b; }
function rem(a: number, b: number): number { return a % b; }
function pow(a: number, b: number): number { return a ** b; }
function neg(a: number): number { return -a; }
function band(a: number, b: number): number { return a & b; }
function bor(a: number, b: number): number { return a | b; }
function bxor(a: number, b: number): number { return a ^ b; }
function shl(a: number, b: number): number { return a << b; }
function sar(a: number, b: number): number { return a >> b; }
function shr(a: number, b: number): number { return a >>> b; }
function bnot(a: number): number { return ~a; }

const show = (x: unknown) => (Object.is(x, -0) ? "-0" : String(x));
const vals = [0, -0, 1, -1, 0.5, -0.5, 1.5, -2.5, 3, 7, -7, 31, 32, 33, 2 ** 31, -(2 ** 31), 2 ** 31 - 1,
  2 ** 32, 2 ** 32 + 5, -(2 ** 32) - 5, 2 ** 53, 2 ** 53 + 2, 1e20, -1e20, 2 ** 63, -(2 ** 63), 2 ** 64 + 2 ** 33,
  1e300, -1e300, Number.MAX_VALUE, Number.MIN_VALUE, Infinity, -Infinity, NaN];
const bin = { add, sub, mul, div, rem, pow, band, bor, bxor, shl, sar, shr } as Record<string, (a: number, b: number) => number>;
for (const name of Object.keys(bin)) {
  const out: string[] = [];
  for (const a of vals) for (const b of vals) out.push(show(bin[name](a, b)));
  console.log(name, out.join(" "));
}
const un = { neg, bnot } as Record<string, (a: number) => number>;
for (const name of Object.keys(un)) console.log(name, vals.map((a) => show(un[name](a))).join(" "));
