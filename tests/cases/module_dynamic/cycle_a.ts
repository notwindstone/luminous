import { b, aFromB } from "./cycle_b.ts";
console.log("cycle_a evaluating");
export function a(): string {
  return "a";
}
export function fromB(): string {
  return b() + aFromB();
}
