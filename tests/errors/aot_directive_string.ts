// @error `aot` function `gen` cannot be compiled natively
// @error unsupported expression: this
async function* gen(n: number) {
  "use aot";
  yield n + (this === undefined ? 0 : 1);
}
console.log(gen(1));
