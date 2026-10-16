// @native fib walk mark isEven isOdd depth deepSum Tree count flatten
// Recursive functions without a return type annotation: native, returning
// any value (whatever their returns are).
function fib(n: number) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

type Node = { value: number; children: Node[] };

function walk(node: Node, out: number[] = []) {
  out.push(node.value);
  for (const c of node.children) walk(c, out);
  return out;
}

// No return value at all.
function mark(node: any, seen: Set<any>) {
  if (seen.has(node)) return;
  seen.add(node);
  for (const k of Object.keys(node)) if (typeof node[k] === "object" && node[k] !== null) mark(node[k], seen);
}

// Mutual recursion.
function isEven(n: number) {
  return n === 0 ? true : isOdd(n - 1);
}
function isOdd(n: number) {
  return n === 0 ? false : isEven(n - 1);
}

// A mix of what it returns: numbers, strings, undefined.
function depth(x: any, d = 0) {
  if (Array.isArray(x)) return x.length === 0 ? "empty" : depth(x[0], d + 1);
  if (x === null) return undefined;
  return d;
}

function deepSum(n: number) {
  if (n === 0) return 0;
  return n + deepSum(n - 1);
}

class Tree {
  left: Tree | null;
  right: Tree | null;
  constructor(left: Tree | null, right: Tree | null) {
    this.left = left;
    this.right = right;
  }
  count() {
    return 1 + (this.left ? this.left.count() : 0) + (this.right ? this.right.count() : 0);
  }
}

function count(t: Tree | null) {
  return t === null ? 0 : 1 + count(t.left) + count(t.right);
}

function flatten(xs: any[]) {
  const out: any[] = [];
  for (const x of xs) {
    if (Array.isArray(x)) for (const y of flatten(x)) out.push(y);
    else out.push(x);
  }
  return out;
}

const tree: Node = { value: 1, children: [{ value: 2, children: [{ value: 3, children: [] }] }, { value: 4, children: [] }] };
console.log(fib(20), typeof fib(3));
console.log(walk(tree).join(","));
const cyc: any = { a: { b: {} } };
cyc.a.b.c = cyc;
const seen = new Set();
console.log(mark(cyc, seen), seen.size);
console.log(isEven(10), isOdd(7), isEven(3));
console.log(depth([[[1]]]), depth([[]]), depth(null), depth(5));
console.log(deepSum(1000));
const t = new Tree(new Tree(null, new Tree(null, null)), new Tree(null, null));
console.log(t.count(), count(t));
console.log(JSON.stringify(flatten([1, [2, [3, [4]], 5], [[6]]])));
try {
  deepSum(1e7);
} catch (e) {
  console.log((e as Error).constructor.name);
}
