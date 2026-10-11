// Binary trees (the Computer Language Benchmarks Game): allocation and
// recursion over small objects.
type Tree = { left: Tree | null; right: Tree | null };

function bottomUp(depth: number): Tree {
  return depth > 0 ? { left: bottomUp(depth - 1), right: bottomUp(depth - 1) } : { left: null, right: null };
}

function check(t: Tree): number {
  return t.left === null ? 1 : 1 + check(t.left) + check(t.right!);
}

const start = Date.now();
const maxDepth = 16;
const stretch = maxDepth + 1;
console.log(`stretch tree of depth ${stretch}\t check: ${check(bottomUp(stretch))}`);
const longLived = bottomUp(maxDepth);
for (let depth = 4; depth <= maxDepth; depth += 2) {
  const iterations = 1 << (maxDepth - depth + 4);
  let sum = 0;
  for (let i = 0; i < iterations; i++) sum += check(bottomUp(depth));
  console.log(`${iterations}\t trees of depth ${depth}\t check: ${sum}`);
}
console.log(`long lived tree of depth ${maxDepth}\t check: ${check(longLived)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
