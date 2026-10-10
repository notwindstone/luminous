// Floating point with libm calls.
function f(x: number): number {
  return Math.sin(x) * Math.exp(-x / 10) + Math.sqrt(x + 1);
}

function simpson(a: number, b: number, n: number): number {
  const h = (b - a) / n;
  let s = f(a) + f(b);
  for (let i = 1; i < n; i++) s += (i % 2 === 0 ? 2 : 4) * f(a + i * h);
  return (s * h) / 3;
}

const start = Date.now();
console.log(`integral = ${simpson(0, 100, 10_000_000).toFixed(9)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
