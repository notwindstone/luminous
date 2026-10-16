// @error `compiled` function `uses` cannot be compiled natively
compiled function uses(): number {
  return this === undefined ? 0 : 1;
}
