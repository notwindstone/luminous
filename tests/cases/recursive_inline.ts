// @native fib walk depthOf sumDown visit failAt down gcd ack countNodes
// A small function that calls itself has its own body inlined once (each
// call does two levels): results, evaluation order, exceptions thrown at
// any depth and deep recursion are unchanged.

function fib(n: number): number {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

type T = { l: T | null; r: T | null; v: number };

function walk(depth: number, v: number): T {
  return depth > 0 ? { l: walk(depth - 1, v * 2), r: walk(depth - 1, v * 2 + 1), v } : { l: null, r: null, v };
}

function depthOf(t: T | null): number {
  if (t === null) return 0;
  const a = depthOf(t.l);
  const b = depthOf(t.r);
  return 1 + (a > b ? a : b);
}

function countNodes(t: T | null): number {
  return t === null ? 0 : 1 + countNodes(t.l) + countNodes(t.r);
}

function sumDown(n: number): number {
  return n <= 0 ? 0 : n + sumDown(n - 1);
}

const order: number[] = [];
function visit(t: T | null): void {
  if (t === null) return;
  order.push(t.v);
  visit(t.l);
  visit(t.r);
}

function failAt(n: number, at: number): number {
  if (n === at) throw new Error("failed at " + n);
  return n === 0 ? 0 : 1 + failAt(n - 1, at);
}

function down(n: number): number {
  return n === 0 ? 0 : 1 + down(n - 1);
}

function gcd(a: number, b: number): number {
  return b === 0 ? a : gcd(b, a % b);
}

function ack(m: number, n: number): number {
  return m === 0 ? n + 1 : n === 0 ? ack(m - 1, 1) : ack(m - 1, ack(m, n - 1));
}

console.log(fib(0), fib(1), fib(2), fib(3), fib(20), fib(25));
const t = walk(6, 1);
console.log(depthOf(t), countNodes(t), depthOf(null));
console.log(sumDown(0), sumDown(1), sumDown(400), sumDown(-3));
visit(walk(3, 1));
console.log(order.join(","));
for (const at of [0, 1, 2, 3, 7, 50]) {
  try {
    console.log(failAt(20, at));
  } catch (e) {
    console.log((e as Error).message);
  }
}
console.log(failAt(9, -1));
try {
  console.log(down(1e7));
} catch (e) {
  console.log("deep:", (e as Error).constructor.name);
}
console.log(down(300));
console.log(gcd(1071, 462), gcd(17, 5), gcd(0, 9), ack(2, 3), ack(3, 3));
