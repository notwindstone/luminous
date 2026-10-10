// @error `Math.sqrt` cannot be reassigned
// @error `Number.isNaN` cannot be deleted
// @error `Object.defineProperty` on `Math`
// @error `globalThis.Math` cannot be reassigned
Math.sqrt = (x: number) => x;
delete (Number as any).isNaN;
Object.defineProperty(Math, "PI", { value: 3 });
(globalThis as any).Math = {};
