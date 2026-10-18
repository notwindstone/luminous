// @native rd rdm many viaObj
// Reads of properties objects do not have, cached per shape, against the
// generic answers.
const log = [];
function rd(o) { return o.q; }
function rdm(o) { return o.status === undefined ? "none" : o.status; }
function many(o) { return String(o.q) + String(o.r); }
const a = { x: 1 }, b = { y: 2 }, c = { status: 5 };
for (let i = 0; i < 5; i++) { rd(a); rd(b); rdm(a); rdm(c); }
log.push("start " + rd(a) + " " + rd(b) + " " + rdm(a) + " " + rdm(c));
// On Object.prototype, then removed.
Object.prototype.q = "proto";
log.push("on Object.prototype " + rd(a) + " " + rd(b));
delete Object.prototype.q;
log.push("removed " + rd(a) + " " + rd(b));
// A getter on Object.prototype.
Object.defineProperty(Object.prototype, "q", { get() { return "getter " + JSON.stringify(this); }, configurable: true });
log.push("getter " + rd(a));
delete Object.prototype.q;
log.push("getter removed " + rd(a));
// Own property added: another shape.
const d = { x: 1 }; rd(d); d.q = "own"; log.push("own " + rd(d) + " " + rd({ x: 1 }));
// Deleted again.
delete d.q; log.push("deleted " + rd(d));
// A class prototype gains it.
class K { constructor() { this.k = 1; } }
const k = new K(); for (let i = 0; i < 3; i++) rd(k);
K.prototype.q = "K proto"; log.push("class proto " + rd(k));
delete K.prototype.q; log.push("class proto gone " + rd(k));
// A superclass prototype gains a getter.
class L extends K {}
const l = new L(); for (let i = 0; i < 3; i++) rd(l);
Object.defineProperty(K.prototype, "q", { get() { return "K getter"; }, configurable: true });
log.push("super getter " + rd(l));
delete K.prototype.q;
// Prototype changed on the object: another shape.
const e = { x: 1 }; rd(e); Object.setPrototypeOf(e, { q: "new proto" }); log.push("setPrototypeOf " + rd(e));
// The prototype object's own prototype changed.
const p1 = { m: 1 }, e2 = Object.create(p1); for (let i = 0; i < 3; i++) rd(e2);
Object.setPrototypeOf(p1, { q: "grandproto" }); log.push("proto's proto " + rd(e2));
// A proxy in the chain.
const traps = [];
const px = new Proxy({}, { get(t, key, r) { traps.push(String(key)); return "trap"; } });
const e3 = Object.create(px); log.push("proxy chain " + rd(e3) + " " + rd(e3) + " " + traps.join());
// Null prototype.
const n = Object.create(null); n.x = 1; for (let i = 0; i < 3; i++) rd(n); log.push("null proto " + rd(n));
// Exotic objects: arrays, typed arrays, strings, arguments.
const arr = [1, 2]; log.push("array " + rd(arr) + " " + many(arr));
Array.prototype.q = "array proto"; log.push("array proto " + rd(arr) + " " + rd([]));
delete Array.prototype.q;
const ta = new Uint8Array(2); log.push("typed " + rd(ta));
function viaObj(o) { return o["-0"]; }
log.push("typed -0 " + viaObj(ta) + " " + viaObj({}));
Object.prototype["-0"] = "neg zero";
log.push("typed -0 after " + viaObj(ta) + " " + viaObj({}));
delete Object.prototype["-0"];
log.push("string " + rd(new String("ab")) + " " + rdm(new String("ab")));
// Polymorphic site: many shapes, some with the property.
let s = "";
for (let i = 0; i < 40; i++) { const o = { ["f" + (i % 9)]: i }; if (i % 4 === 0) o.q = i; s += rd(o) === undefined ? "u" : "d"; }
log.push("poly " + s);
// Frozen and sealed objects: the same answers.
log.push("frozen " + rd(Object.freeze({ x: 1 })) + " " + rdm(Object.freeze({ status: 3 })));
console.log(log.join("\n"));
