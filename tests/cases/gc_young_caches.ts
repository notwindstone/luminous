// @native get getm
// @env QJS_GC_YOUNG=64 QJS_GC_STRESS=0
// Inline caches holding shapes made all along (young ones) while minor
// collections run often (a small young generation): the shapes stay alive
// while the caches hold them, and the caches stay cheap to mark. Classes
// with static blocks also keep closures over their `this` whose variable
// is closed after a collection made the closure old.
function get(o: any): number { return o.x + (o.y || 0); }
function getm(o: A): number { return o.m(); }
class A { constructor(public x: number) {} m(): number { return this.x; } }
class B extends A { m(): number { return this.x * 2; } }
let sum = 0;
for (let round = 0; round < 200; round++) {
  // New shapes each round: properties named by the round.
  const objs: any[] = [];
  for (let i = 0; i < 50; i++) {
    const o: any = { x: i };
    o["k" + (round % 37)] = i;
    if (i & 1) o.y = round;
    objs.push(o);
  }
  for (const o of objs) sum += get(o);
  for (let i = 0; i < 20; i++) sum += getm(i & 1 ? new A(i) : new B(i));
  // Garbage.
  for (let i = 0; i < 2000; i++) [{ a: i }, "s" + i];
}
console.log(sum);
