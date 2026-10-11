// @error `a` is an `int`, but this value may leave the int32 range
// @error `b` is an `int`, but this value is a number that may not be an int32
// @error `i` is an `int`, but this value may leave the int32 range
// @error `u` is an `int`, but this value is a number that may not be an int32
// @error `s` is an `int`, but this value is not a number
// @error an `int` variable needs an initializer
// @error the function returns `int`, but this value may not be an int32
// @error `n` is an `int`, but this value may leave the int32 range
type int = number;

function sum(n: int): int {
  const a: int = n + 1;
  return a;
}

function half(): number {
  const b: int = 0.5;
  return b;
}

function unbounded(n: int): number {
  let s = 0;
  for (let i: int = 0; i <= n; i++) s += i;
  return s;
}

function unsigned(x: int): int {
  const u: int = x >>> 0;
  return u;
}

function str(): number {
  const s: int = "1" as unknown as int;
  return s;
}

function uninit(): number {
  let c: int;
  c = 1;
  return c;
}

function ratio(n: int): int {
  return n / 2;
}

function param(n: int): int {
  n++;
  return n;
}

console.log(sum(1), half(), unbounded(3), unsigned(1), str(), uninit(), ratio(4), param(1));
