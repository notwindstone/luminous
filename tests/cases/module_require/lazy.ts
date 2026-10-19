console.log("lazy.ts evaluates");
export const value = 42;
export let counter = 0;
export function bump() {
  counter++;
}
export default function plusOne(n: number) {
  return n + 1;
}
