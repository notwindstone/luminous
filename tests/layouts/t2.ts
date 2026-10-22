// 1 setter stores, later mutated through the getter
class Holder { #v: any = null; set v(x: any) { this.#v = x; } get v() { return this.#v; } }
const h = new Holder();
h.v = { s1: 1 };
(h.v as any).more = 1;

// 2 this via call / bind
function addKey(this: any) { this.added = 1; }
const viaCall = { c1: 1 };
addKey.call(viaCall);
const viaBind = { b1: 1 };
addKey.bind(viaBind)();
const viaApply = { a1: 1 };
Reflect.apply(addKey, viaApply, []);

// 3 generator yields
function* gen() { yield { g1: 1 }; }
for (const o of gen()) (o as any).x = 1;

// 4 custom iterator object
const iterable = { [Symbol.iterator]() { let done = false; return { next() { const r = { value: { it1: 1 }, done }; done = true; return r; } }; } };
for (const o of iterable) (o as any).y = 1;

// 5 class extends Map: values through super methods
class MyMap extends Map<string, any> { put(k: string, v: any) { this.set(k, v); } }
const mm = new MyMap();
mm.put("k", { mm1: 1 });
mm.get("k").z = 1;

// 6 Error cause
const cause = { c: 1 };
try { throw new Error("x", { cause }); } catch (e: any) { e.cause.extra = 1; }

// 7 thrown literal
try { throw { thrown: 1 }; } catch (e: any) { e.more = 2; }

// 8 Object.create proto chain: writes to the proto via the child are own
const proto = { greet() { return 1; } };
const child = Object.create(proto);
child.own = 1;
Object.getPrototypeOf(child).added = 1;

// 9 Object.values / entries
const vals = { inner: { deep: 1 } };
for (const v of Object.values(vals)) (v as any).w = 1;

// 10 arguments and rest
function viaArgs() { (arguments[0] as any).viaArgs = 1; }
viaArgs({ ar1: 1 });
function viaRest(...xs: any[]) { xs[0].viaRest = 1; }
viaRest({ r1: 1 });
function viaSpread(a: any, b: any) { b.viaSpread = 1; }
viaSpread(...[{ sp0: 1 }, { sp1: 1 }]);

// 11 promise resolve + thenable
const pr = new Promise<any>((res) => res({ p1: 1 }));
pr.then((o) => { o.q = 1; });
const thenable = { then(r: (v: any) => void) { r({ th1: 1 }); } };
(async () => { const v: any = await thenable; v.t = 1; })();

// 12 callback given to unknown code: its return escapes
declare const unknownLib: any;
unknownLib.register(() => ({ cb1: 1 }));

// 13 stored into array then mutated via index
const arr: any[] = [];
arr.push({ arr1: 1 });
arr[0].k = 1;

// 14 Reflect.set and defineProperty
const rs = { r: 1 };
Reflect.set(rs, "r", 2);
const dp = { d: 1 };
Object.defineProperty(dp, "d", { value: 2 });


// 16 export
export const exported = { ex: 1 };

// 17 JSON.stringify with and without replacer
const js1 = { j1: 1 };
JSON.stringify(js1);
const js2 = { j2: 1 };
JSON.stringify(js2, (k, v) => v);

// 18 setTimeout passes args
setTimeout((o: any) => { o.later = 1; }, 0, { st1: 1 });

// 19 closures capture and mutate
function outer() { const cap = { cap1: 1 }; return () => { (cap as any).add = 1; }; }
outer()();

// 20 symbols as keys
const sym = Symbol("s");
const symObj = { a: 1 };
(symObj as any)[sym] = 1;

// 21 array destructuring and default
const [first = { def1: 1 }] = [] as any[];
first.k = 1;

// 22 getter on a literal is an accessor
const acc = { get x() { return 1; } };

// 23 Function.prototype.call via uncurry
const uncurry = Function.prototype.call.bind(Array.prototype.push);
const target: any[] = [];
uncurry(target, { un1: 1 });
target[0].k = 1;

// 24 Object.setPrototypeOf with literal proto then write through inst
const protoLit = { pl: 1 };
const inst = {};
Object.setPrototypeOf(inst, protoLit);

// 25 method stored in a literal called with this
const counter = { n: 0, inc() { this.n++; }, bad() { (this as any).m = 1; } };
counter.inc();
