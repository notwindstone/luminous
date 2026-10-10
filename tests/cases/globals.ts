// @native readGlobal writeGlobal missing typeofMissing useConsole
(globalThis as Record<string, unknown>).shared = 1;
aot function readGlobal(): unknown {
  return (globalThis as Record<string, unknown>).shared;
}
aot function writeGlobal(v: number): void {
  // @ts-ignore
  shared = v;
}
aot function missing(): unknown {
  // @ts-ignore
  return notDefinedAnywhere;
}
aot function typeofMissing(): string {
  // @ts-ignore
  return typeof notDefinedAnywhere;
}
aot function useConsole(): void {
  console.log("from native", 1, { a: [1, 2] });
}
console.log(readGlobal());
writeGlobal(42);
console.log(readGlobal(), typeofMissing());
try {
  missing();
} catch (e) {
  console.log((e as Error).name, (e as Error).message);
}
useConsole();
