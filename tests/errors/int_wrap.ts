// @error `a` is an `int`, but this value may leave the int32 range (only `++`
// @error `b` is an `int`, but this value is a number that may not be an int32
// @error the function returns `int`, but this value may not be an int32
type int = number;

function plain(n: int): int {
  let a: int = 0;
  a = n * 2;
  return a;
}

function divide(n: int): int {
  let b: int = n;
  b /= 2;
  return b;
}

function ret(n: int): int {
  return n + 1;
}

console.log(plain(1), divide(2), ret(4));
