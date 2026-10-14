import { fromB, later, bValue } from "./b";
type int = number;
export function fromA(n: int): int { return n * 2; }
export let aCount: int = 0;
const console2 = 1;
export function readLater(): string {
  try { return String(later); } catch (e) { return (e as Error).constructor.name; }
}
console.log("a body", fromB(3), bValue, console2);
aCount++;
