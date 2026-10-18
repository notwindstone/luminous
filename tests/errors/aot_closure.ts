// @error `aot` function `f` cannot be compiled natively
// @error unsupported expression: new.target
aot function f(x: number): number {
  const g = function () { return new.target; };
  return x + (g() ? 1 : 0);
}
