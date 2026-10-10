// @native add flag twice
function add(a: number, b: number): number { return a + b; }
function flag(b: boolean): number { return b ? 1 : 2; }
function twice(x: number): number { return add(x, x); }

// Arguments that violate the declared types fall back to the interpreter.
const anyAdd = add as unknown as (...args: unknown[]) => unknown;
const anyFlag = flag as unknown as (...args: unknown[]) => unknown;
console.log(anyAdd(1, 2), anyAdd(1.5, 2 ** 40), anyAdd("a", "b"), anyAdd(1), anyAdd(), anyAdd(1, 2, 3), anyAdd({}, 1));
console.log(anyAdd(1n, 2n), anyAdd(null, true), anyAdd([1], [2]));
console.log(anyFlag(true), anyFlag(false), anyFlag(1), anyFlag(0), anyFlag("x"), anyFlag());
console.log(twice(21), add.name, add.length, flag.name, typeof add);
console.log([1, 2, 3].map(twice).join(","));
console.log(add.call(null, 5, 6), add.apply(null, [7, 8]));
console.log(Reflect.apply(add, undefined, ["x", 1]));
