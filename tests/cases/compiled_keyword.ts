// @native add twice viaDirective sum
// `compiled` is another name for `aot`, for functions and types; elsewhere
// it is an ordinary identifier.

compiled type V = { x: int; y: number };

compiled function add(a: number, b: number): number {
  return a + b;
}

const twice = compiled (x: number): number => x * 2;

function viaDirective(n: number): number {
  "use compiled";
  return n + 1;
}

function sum(n: int): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const v: V = { x: i, y: i / 2 };
    s += v.x + v.y;
  }
  return s;
}

// Not markers.
const compiled = 5;
function useIt(f: (n: number) => number): number {
  return f(compiled);
}
const obj = { compiled: 1, other: compiled + 1 };

const v: V = { x: 1, y: 2 };
console.log(add(1, 2), twice(4), viaDirective(1), sum(10), useIt((n) => n * 3), JSON.stringify(obj));
console.log(JSON.stringify(v), Object.isExtensible(v));
