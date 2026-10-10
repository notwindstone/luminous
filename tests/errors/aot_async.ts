// @error async functions are not supported yet
aot async function f(x: number): Promise<number> {
  return x;
}
