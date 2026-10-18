// @error `aot` function `count` cannot be compiled natively
// @error unsupported expression: new.target
const count = aot function (): number {
  return new.target === undefined ? 0 : 1;
};
