// @native depth sum
// Stack depth is implementation-defined: native frames are far smaller than
// interpreter frames, so only depths that fit in both are compared exactly.
function depth(n: number): number {
  if (n === 0) return 0;
  return 1 + depth(n - 1);
}
function sum(n: number): number {
  return n <= 0 ? 0 : n + sum(n - 1);
}
console.log(depth(500), sum(500));
try {
  console.log(depth(1e8));
} catch (e) {
  console.log(`${(e as Error).name}: ${(e as Error).message}`);
}
// The runtime is still usable after unwinding.
console.log(depth(10), sum(100));
