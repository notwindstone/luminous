// Worst case for native code: a tiny native function called from an
// interpreted loop, so every call pays the guard + boxing cost.
function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * t;
}

const start = Date.now();
let acc = 0;
for (let i = 0; i < 5_000_000; i++) acc += lerp(i, i + 1, 0.25);
console.log(`acc = ${acc}`);
console.log(`elapsed: ${Date.now() - start} ms`);
