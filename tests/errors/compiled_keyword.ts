// @error `compiled` function `uses` cannot be compiled natively
// @error unsupported expression: new.target
compiled function uses(): number {
  return new.target === undefined ? 0 : 1;
}
