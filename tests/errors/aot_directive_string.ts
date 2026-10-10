// @error `aot` function `gen` cannot be compiled natively
// @error async generators are not supported yet
async function* gen(n: number) {
  "use aot";
  yield n;
}
console.log(gen(1));
