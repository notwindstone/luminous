// @error `aot` function `gen` cannot be compiled natively
// @error `arguments` is not supported
async function* gen(n: number) {
  "use aot";
  yield n + arguments.length;
}
console.log(gen(1));
