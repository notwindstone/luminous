export const console = { log: (...xs: unknown[]) => globalThis.console.log("[c]", ...xs) };
export const x = 10;
export const y = 20;
export const { p, q = 5 } = { p: 1 } as { p: number; q?: number };
export default class { hello(): string { return "anon class"; } }
