export const unused = 1;
const g = globalThis as unknown as { sideEffects?: string[] };
(g.sideEffects ||= []).push("side ran");
