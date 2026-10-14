export let counter = 0;
export function bump(n: number): number { counter += n; return counter; }
function helper(): string { return "counter-helper"; }
export default function makeLabel(s: string): string { return `${helper()}:${s}`; }
console.log("counter loaded");
