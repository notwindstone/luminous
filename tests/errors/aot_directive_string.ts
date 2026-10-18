// @error `aot` function `gen` cannot be compiled natively
// @error unsupported expression: new.target
async function* gen(n: number) {
  "use aot";
  yield n + (new.target === undefined ? 0 : 1);
}
console.log(gen(1));
