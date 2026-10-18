// @error `aot` function `f` cannot be compiled natively
// @error unsupported expression: new.target
aot async function* f(n: number): AsyncGenerator<number> {
  yield n + (new.target === undefined ? 0 : 1);
}
console.log(f(1));
