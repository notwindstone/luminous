// @native fib sq cube twice clamp exported viaConst later aot usesLater

// The `aot` keyword and the "use aot" directive force native compilation.
aot function fib(n: number): number {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

const sq = aot (x: number): number => x * x;
const cube = aot function (x: number): number { return x * x * x; };
const twice = (x: number): number => {
  "use aot";
  return 2 * x;
};
function clamp(x: number, lo: number, hi: number): number {
  "use aot";
  return Math.min(Math.max(x, lo), hi);
}
export aot function exported(x: number): number { return sq(x) + cube(x); }
// Unmarked const arrows are compiled on a best-effort basis too.
const viaConst = (x: number): number => twice(sq(x));

// `aot` is still an ordinary identifier everywhere else.
function aot(x: number) { return `aot(${x})`; }
const label = "aot function inside a string";
/* aot function inside a comment */
console.log(aot (1), aot(2), label);

console.log(fib(20), sq(7), cube(3), twice(21), clamp(15, 0, 10), exported(2), viaConst(3));
console.log(sq.name, sq.length, cube.name, typeof cube);
// Fallbacks keep working for marked functions.
console.log((sq as unknown as (x: unknown) => unknown)("ab"), (clamp as unknown as () => unknown)());

// A const function declared after top-level code has run is native, and
// native callers reach it through its TDZ-checked binding instead of a
// direct call.
function usesLater(x: number): number { return later(x) * 10; }
console.log("tdz boundary");
const later = (x: number): number => x + 1;
console.log(later(1), usesLater(1));
