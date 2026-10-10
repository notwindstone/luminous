function fib(n: number): number {
  if (n < 2) return n;
  return fib(n - 1) + fib(n - 2);
}

const start = Date.now();
const result = fib(32);
console.log(`fib(32) = ${result}`);
console.log(`elapsed: ${Date.now() - start} ms`);
