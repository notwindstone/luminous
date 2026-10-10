// Array and typed-array element access in hot loops.

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

function dot(a: Float64Array, b: Float64Array): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i] * b[i];
  return s;
}

function fill(a: Float64Array): void {
  for (let i = 0; i < a.length; i++) a[i] = Math.sin(i);
}

function sumArray(xs: number[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += xs[i];
  return s;
}

function prefixSums(xs: number[]): void {
  for (let i = 1; i < xs.length; i++) xs[i] = xs[i] + xs[i - 1];
}

const start = Date.now();
console.log(`primes below 5e6: ${sieve(5_000_000)}`);
const a = new Float64Array(1_000_000);
const b = new Float64Array(1_000_000);
fill(a);
fill(b);
let d = 0;
for (let k = 0; k < 10; k++) d += dot(a, b);
console.log(`dot: ${d.toFixed(6)}`);
const xs: number[] = [];
for (let i = 0; i < 1_000_000; i++) xs.push(i % 7);
prefixSums(xs);
let t = 0;
for (let k = 0; k < 10; k++) t += sumArray(xs);
console.log(`sum: ${t}`);
console.log(`elapsed: ${Date.now() - start} ms`);
