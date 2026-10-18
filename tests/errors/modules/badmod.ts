export function ok(): number {
  return 1;
}
export aot function f(n: number): number {
  return n + (new.target === undefined ? 0 : 1);
}
