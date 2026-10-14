import { a } from "./cycle_a.ts";
console.log("cycle_b evaluating, a() is", a());
export function b(): string {
  return "b";
}
export function aFromB(): string {
  return a();
}
