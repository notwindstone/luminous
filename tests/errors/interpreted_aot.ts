// @error `aot` function `outer` cannot be compiled natively
// @error contains a function marked `interpreted`
aot function outer(): number {
  const inner = interpreted (): number => 1;
  return inner();
}
console.log(outer());
