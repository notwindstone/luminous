// @error `aot` function `gen` cannot be compiled natively
// @error generators are not supported yet
function* gen(n: number) {
  "use aot";
  yield n;
}
