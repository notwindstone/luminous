// @native check
// instanceof through the per-site cache, against the generic answers.
class A {}
class B extends A {}
function F() {}
const log = [];
function check(v, C) { return v instanceof C; }
function run(tag) {
  const vals = [new A(), new B(), new F(), {}, 1, "s", null, undefined, Object.create(B.prototype), [], () => 0];
  const ctors = [A, B, F, Object, Array, Function];
  let s = "";
  for (const C of ctors) for (const v of vals) s += check(v, C) ? "1" : "0";
  log.push(tag + " " + s);
}
for (let i = 0; i < 3; i++) run("plain");
// F.prototype replaced (writable): the slot's value is read each time.
F.prototype = A.prototype;
run("F.prototype=A.prototype");
F.prototype = 5;
try { check({}, F); log.push("no throw"); } catch (e) { log.push("throws " + e.constructor.name); }
check(1, F) ? log.push("1 inst") : log.push("1 not");
F.prototype = {};
run("F.prototype={}");
// A @@hasInstance of its own: the shape changes.
Object.defineProperty(B, Symbol.hasInstance, { value: (v) => v === 7, configurable: true });
log.push("B own hasInstance " + check(7, B) + " " + check(new B(), B));
delete B[Symbol.hasInstance];
log.push("B deleted " + check(7, B) + " " + check(new B(), B));
// On a superclass constructor (B's prototype): watched.
Object.defineProperty(A, Symbol.hasInstance, { value: (v) => v === 8, configurable: true });
log.push("A has, via B " + check(8, B) + " " + check(new B(), B) + " " + check(8, A));
delete A[Symbol.hasInstance];
log.push("A deleted " + check(8, B) + " " + check(new B(), B));
// On Function.prototype itself, before it: a new object in the chain.
const P = Object.create(Function.prototype);
Object.defineProperty(P, Symbol.hasInstance, { value: () => true });
function G() {}
log.push("G plain " + check({}, G));
Object.setPrototypeOf(G, P);
log.push("G via P " + check({}, G));
// A proxy on the prototype chain, and as the operand.
const prox = new Proxy({}, { getPrototypeOf() { log.push("trap"); return A.prototype; } });
log.push("proxy value " + check(prox, A));
const objWithProxyProto = Object.create(prox);
log.push("proxy in chain " + check(objWithProxyProto, A));
const pctor = new Proxy(A, {});
log.push("proxy ctor " + check(new A(), pctor));
// A bound function.
const bound = A.bind(null);
log.push("bound " + check(new A(), bound) + " " + check({}, bound));
// Not callable, though shaped like a function.
const fake = Object.create(Function.prototype);
Object.defineProperty(fake, "length", { value: 0, configurable: true });
Object.defineProperty(fake, "name", { value: "A", configurable: true });
Object.defineProperty(fake, "prototype", { value: A.prototype, writable: false });
log.push("fake " + check(new A(), fake));
try { check({}, 3); } catch (e) { log.push("rhs 3: " + e.constructor.name); }
try { check({}, {}); } catch (e) { log.push("rhs {}: " + e.constructor.name); }
// Many constructors at one site.
let n = 0;
for (let i = 0; i < 200; i++) { const K = class {}; if (check(new K(), K)) n++; if (check({}, K)) n += 100; }
log.push("many " + n);
console.log(log.join("\n"));
