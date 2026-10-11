// @error `a` is an `int`, but this value is not an integer
// @error `b` is an `int`, but this value is not an integer
// @error `c` is an `int`, but this value may not be an integer
// @error `d` is an `int`, but this value may not be an integer
// @error `e` is an `int`, but this value is not known to be an integer
// @error `f` is an `int`, but this value is an integer literal out of the int32 range
// @error the function returns `int`, but this value may not be an integer
// @error the function returns `int`, but this value is `undefined`
// @error `g` is an `int`, but this value is not known to be an integer
type int = number;

function fractions(n: int): number {
  let a: int = n;
  a = a + 1.5;
  let b: int = n;
  b += 0.5;
  let c: int = n;
  c /= 2;
  let d: int = n;
  d = d % 3;
  return a + b + c + d;
}

function untyped(xs: number[]): number {
  let e: int = 0;
  e = xs[0];
  let f: int = 1e20;
  return e + f;
}

function ratio(n: int): int {
  return Math.floor(n / 2);
}

function nothing(): int {
  return;
}

const g: int = Number("5");

console.log(fractions(1), untyped([1]), ratio(4), nothing(), g);
