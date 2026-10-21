// @native run
// Symbol keys of computed member reads and writes: the fast path of atom
// strings (data properties of ordinary objects), else the general one
// (setters, frozen objects).
const kFd = Symbol("kFd"), kRefs = Symbol("kRefs"), g = Symbol.for("global");
class H { [k: symbol]: any; constructor() { (this as any)[kFd] = 3; (this as any)[kRefs] = 0; } }
function run(n: number): string {
  const h: any = new H();
  let s = 0;
  for (let i = 0; i < n; i++) { h[kRefs]++; s += h[kFd]; h[g] = i; }
  const proto: any = Object.getPrototypeOf(h);
  proto[Symbol.toStringTag] = "Hh";
  const frozen: any = Object.freeze({ [kFd]: 1 });
  let err = "";
  try { "use strict"; frozen[kFd] = 2; } catch (e) { err = (e as Error).constructor.name; }
  const withSetter: any = { set [kFd](v: number) { s += 1000 * v; } };
  withSetter[kFd] = 5;
  return [s, h[kRefs], h[g], String(h), frozen[kFd], err, Object.getOwnPropertySymbols(h).map(String).join("+")].join(" ");
}
console.log(run(10000));
