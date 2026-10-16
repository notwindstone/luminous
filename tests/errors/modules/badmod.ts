export function ok(): number {
  return 1;
}
export aot function f(n: number): number {
  return n + (this === undefined ? 0 : 1);
}
