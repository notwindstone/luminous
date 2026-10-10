// Integer-heavy: bitwise ops exercise the ToInt32 paths.
function hashRounds(seed: number, rounds: number): number {
  let x = seed | 0;
  let acc = 0;
  for (let i = 0; i < rounds; i++) {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    acc = (acc + (x >>> 0) % 1000) | 0;
  }
  return acc;
}

const start = Date.now();
console.log(`hash = ${hashRounds(2463534242, 20_000_000)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
