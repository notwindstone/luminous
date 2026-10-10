// @error `aot` function `f` cannot be compiled natively
// @error unsupported expression: closure
aot function f(x: number): number {
  const g = () => x;
  return g();
}
