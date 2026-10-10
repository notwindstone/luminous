// @error async generators are not supported yet
aot async function* f(n: number): AsyncGenerator<number> {
  yield n;
}
