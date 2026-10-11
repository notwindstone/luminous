// @error `w` is not a field of `aot type` `P`
// @error not an object of `aot type` `P`: missing `y`
// @error `x` of `P` cannot hold this value
// @error a spread is not allowed in an object of `aot type` `P`
// @error not an object of `aot type` `K`: missing `color`
// @error `x` is set twice
type int = number;

aot type P = { x: int; y: number };
aot type K = { kind: "a" } | { kind: "b"; color: string };

const extra: P = { x: 1, y: 2, w: 3 };
const missing: P = { x: 1 };
const fraction: P = { x: 1.5, y: 2 };
const spread: P = { ...extra };
const noColor: K = { kind: "b" };
const twice: P = { x: 1, x: 2, y: 3 };
