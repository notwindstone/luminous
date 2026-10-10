// @error `aot` function `count` cannot be compiled natively
// @error `arguments` is not supported
const count = aot function (): number {
  return arguments.length;
};
