// @error `aot` function `f` cannot be compiled natively
// @error unsupported expression: this
aot function f(x: number): number {
  const g = function () { return this; };
  return x + (g() ? 1 : 0);
}
