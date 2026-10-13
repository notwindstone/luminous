// @native sumArray maxOf sumMixed dot sumU8 sumI32 sieve fill writeOdd alias clamp
// Numeric regions over loops that read numbers from arrays: an `Array`'s
// element is checked in its iteration (a number, inside the array), else
// the original loop goes on from that iteration (a string, `null`, a hole);
// a typed array's `length` is its element count while its shape is
// pristine, its elements are read in place inside it (fresh after writes,
// through other views of the buffer too), and writes outside it (negative,
// fractional, past the end) do nothing.

function sumArray(xs: number[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += xs[i];
  return s;
}

function maxOf(xs: number[]): number {
  let m = -Infinity;
  for (let i = 0; i < xs.length; i++) if (xs[i] > m) m = xs[i];
  return m;
}

function sumMixed(xs: any[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += xs[i];
  return s;
}

function dot(a: Float64Array, b: Float64Array): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i] * b[i];
  return s;
}

function sumU8(a: Uint8Array): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i];
  return s;
}

function sumI32(a: Int32Array, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += a[i];
  return s;
}

function sieve(n: number): number {
  const composite = new Uint8Array(n + 1);
  let count = 0;
  for (let i = 2; i <= n; i++) {
    if (composite[i] === 0) {
      count++;
      for (let j = i * i; j <= n; j += i) composite[j] = 1;
    }
  }
  return count;
}

function fill(a: Float64Array, k: number): void {
  for (let i = 0; i < a.length; i++) a[i] = Math.sin(i) * k;
}

function writeOdd(a: Float64Array): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) {
    a[i - 1] = i;
    a[i + 0.5] = 99;
    a[i] = a[i] + 1;
    s += a[i];
  }
  return s;
}

function alias(a: Float64Array, b: Float64Array): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) {
    b[i] = i * 2;
    s += a[i];
  }
  return s;
}

function clamp(a: Uint8ClampedArray): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) {
    a[i] = i * 100 - 50.5;
    s += a[i];
  }
  return s;
}

const xs: number[] = [];
for (let i = 0; i < 1000; i++) xs.push(i % 7 === 0 ? i * 0.5 : i);
console.log(sumArray(xs), maxOf(xs));
console.log(sumMixed([1, 2, 3.5, "4", 5, null, 7]), sumMixed([1, 2, 3]));
const holes: number[] = [1, 2, 3];
holes[10] = 4;
console.log(sumArray(holes), sumArray([1, 2, 3, 4, 5]), maxOf([]), maxOf([3, NaN, 2]));
console.log(Object.is(maxOf([-0, 0]), -0));

const a = new Float64Array(10), b = new Float64Array(10);
for (let i = 0; i < 10; i++) {
  a[i] = i * 0.5;
  b[i] = i + 1;
}
console.log(dot(a, b), dot(a, new Float64Array(5)));
console.log(sumU8(new Uint8Array([1, 2, 255, 7])));
const w = new Int32Array([-5, 10, 2147483647, -2147483648]);
console.log(sumI32(w, 4), sumI32(w, 6));
const weird = new Float64Array(3);
Object.defineProperty(weird, "length", { value: 2 });
weird[0] = 1;
weird[1] = 2;
weird[2] = 4;
console.log(dot(weird, weird));

console.log(sieve(1000), sieve(2), sieve(0));
const f = new Float64Array(6);
fill(f, 2);
console.log(Array.from(f).map((x) => x.toFixed(4)).join(","));
const g = new Float64Array(5);
console.log(writeOdd(g), Array.from(g).join(","));
const buf = new ArrayBuffer(64);
console.log(alias(new Float64Array(buf), new Float64Array(buf, 8, 4)));
console.log(clamp(new Uint8ClampedArray(5)));
