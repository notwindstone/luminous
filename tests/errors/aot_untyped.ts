// @error `aot` function `count` cannot be compiled natively
// @error unsupported expression: this
const count = aot function (): number {
  return this === undefined ? 0 : 1;
};
