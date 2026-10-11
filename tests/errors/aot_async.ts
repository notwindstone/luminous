// @error `aot` function `f` cannot be compiled natively
// @error `arguments` is not supported
aot async function* f(n: number): AsyncGenerator<number> {
  yield n + arguments.length;
}
console.log(f(1));
