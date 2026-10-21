console.log("lazy.ts evaluates");
export const value = 42;
export let counter = 0;
export function bump() {
  counter++;
}
export default function plusOne(n: number) {
  return n + 1;
}
// (Named as the main module's own: renamed, and still named as written.)
export function fmt(): string {
  return "lazy fmt";
}
export class Point {}
export const n = (): number => 0;
