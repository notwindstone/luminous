// @native makePoint norm move bump keys hasKey remove getter
aot function makePoint(x: number, y: number) {
  return { x, y, tag: "pt", nested: { depth: 1 } };
}
aot function norm(p: { x: number; y: number }): number {
  return Math.sqrt(p.x * p.x + p.y * p.y);
}
aot function move(p: { x: number; y: number }, dx: number): void {
  p.x += dx;
  p.y = p.y * 2;
}
aot function bump(o: Record<string, number>, k: string): number {
  o[k]++;
  return ++o[k];
}
aot function keys(o: object): string {
  return Object.keys(o).join(",");
}
aot function hasKey(o: object, k: string): boolean {
  return k in o;
}
aot function remove(o: Record<string, unknown>, k: string): boolean {
  return delete o[k];
}
aot function getter(o: { v: number }): number {
  return o.v + o.v;
}
let reads = 0;

const p = makePoint(3, 4);
console.log(JSON.stringify(p), norm(p), norm({ x: "3", y: 4 } as never));
move(p, 1);
console.log(p.x, p.y, p.nested.depth, p.tag);
const c: Record<string, number> = { a: 1 };
console.log(bump(c, "a"), bump(c, "b"), JSON.stringify(c));
console.log(keys(p), hasKey(p, "x"), hasKey(p, "z"), remove(c, "a"), JSON.stringify(c));
// Getters run interpreted code from native code, in order.
const g = { get v() { return ++reads; } };
console.log(getter(g), reads);
try {
  remove(Object.freeze({ a: 1 }), "a");
} catch (e) {
  console.log((e as Error).name);
}
try {
  norm(null as never);
} catch (e) {
  console.log((e as Error).name, (e as Error).message);
}
