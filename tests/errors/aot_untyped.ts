// @error `aot` function `count` cannot be compiled natively
// @error rest parameters are not supported yet
const count = aot (...xs: number[]): number => xs.length;
