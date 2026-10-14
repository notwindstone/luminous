import { fromA, readLater, aCount } from "./a.js";
type int = number;
export function fromB(n: int): int { return fromA(n) + 1; }
function peekA(): string {
  try { return String(aCount); } catch (e) { return (e as Error).constructor.name; }
}
console.log("b body", fromA(5), readLater(), peekA());
export const bValue = "b-value";
export let later = "ready";
