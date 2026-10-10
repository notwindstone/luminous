function isPrime(n: number): boolean {
  if (n < 2) return false;
  if (n % 2 === 0) return n === 2;
  for (let d = 3; d * d <= n; d += 2) {
    if (n % d === 0) return false;
  }
  return true;
}

function countPrimes(limit: number): number {
  let count = 0;
  for (let n = 0; n < limit; n++) if (isPrime(n)) count++;
  return count;
}

const start = Date.now();
console.log(`primes below 2e6: ${countPrimes(2_000_000)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
