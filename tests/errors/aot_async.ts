// @error `aot` function `f` cannot be compiled natively
// @error unsupported expression: this
aot async function* f(n: number): AsyncGenerator<number> {
  yield n + (this === undefined ? 0 : 1);
}
console.log(f(1));
