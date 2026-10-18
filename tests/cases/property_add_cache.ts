// @native add add2
// Property additions through a write site's cache, against the generic answers.
const log = [];
function add(o, v) { try { o.x = v; } catch (e) { log.push("threw " + e.constructor.name); } return o; }
function add2(o, v) { o.x = v; o.y = v + 1; o.z = v + 2; o.w = v + 3; o.u = v + 4; o.t = v + 5; return o; }
function show(o) { return JSON.stringify(Object.getOwnPropertyDescriptors(o)); }
for (let i = 0; i < 5; i++) add({}, i);
log.push("plain " + show(add({}, 1)) + " " + show(add({ a: 1 }, 2)));
// Many properties: the property array grows.
for (let i = 0; i < 5; i++) add2({ a: 0 }, i);
log.push("grow " + show(add2({ a: 0 }, 10)));
// Not extensible, sealed, frozen: no addition (strict: throws).
const ne = Object.preventExtensions({});
add(ne, 3); log.push("prevented " + show(ne) + " " + Object.isExtensible(ne));
const fr = Object.freeze({ a: 1 }); add(fr, 3); log.push("frozen " + show(fr));
// A setter on Object.prototype, added later.
let seen = [];
Object.defineProperty(Object.prototype, "x", { set(v) { seen.push(v); }, configurable: true });
add({}, 4); add({ a: 1 }, 5);
log.push("setter " + JSON.stringify(seen) + " " + show(add({}, 6)));
delete Object.prototype.x;
log.push("deleted " + show(add({}, 7)));
// Read-only on a class prototype, added later.
class C { constructor() { this.k = 1; } }
for (let i = 0; i < 3; i++) add(new C(), i);
Object.defineProperty(C.prototype, "x", { value: 9, writable: false, configurable: true });
const c = new C(); add(c, 8); log.push("readonly " + show(c) + " " + c.x);
delete C.prototype.x;
log.push("readonly gone " + show(add(new C(), 10)));
// A setter on a deeper prototype.
class D extends C {}
for (let i = 0; i < 3; i++) add(new D(), i);
Object.defineProperty(C.prototype, "x", { set(v) { log.push("D setter " + v); }, configurable: true });
add(new D(), 11);
delete C.prototype.x;
// A proxy with an empty null-prototype shape, like Object.create(null).
for (let i = 0; i < 3; i++) add(Object.create(null), i);
const traps = [];
const px = new Proxy(Object.create(null), { set(t, k, v) { traps.push(k + "=" + v); t[k] = v * 10; return true; } });
add(px, 12); log.push("proxy " + traps.join() + " " + JSON.stringify(px.x));
// An array (exotic) and a function.
const arr = []; add(arr, 13); log.push("array " + arr.x + " " + arr.length);
function f() {} add(f, 14); log.push("function " + f.x);
// The object is a prototype someone caches methods of.
const proto = { m() { return 1; } };
const kid = Object.create(proto);
function callm(o) { return o.m() + (o.x || 0); }
for (let i = 0; i < 3; i++) callm(kid);
add(proto, 15); log.push("proto add " + callm(kid));
// A prototype in the chain made a proxy later (setPrototypeOf).
class E {}
for (let i = 0; i < 3; i++) add(new E(), i);
Object.setPrototypeOf(E.prototype, new Proxy({}, { set(t, k, v, r) { log.push("chain proxy set " + k); return false; } }));
const e = new E(); add(e, 16); log.push("chain proxy " + show(e));
// Getter only on proto (no setter): throws.
class G {}
for (let i = 0; i < 3; i++) add(new G(), i);
Object.defineProperty(G.prototype, "x", { get() { return "g"; }, configurable: true });
const g = new G(); add(g, 17); log.push("getter only " + show(g) + " " + g.x);
// Many shapes at one site.
let tot = 0;
for (let i = 0; i < 100; i++) { const o = {}; o["p" + (i % 13)] = i; tot += add(o, i).x; }
log.push("many " + tot);
console.log(log.join("\n"));
