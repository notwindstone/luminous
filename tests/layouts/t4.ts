// 1 function class with prototype methods
function Foo(this: any) { this.data = { fd: 1 }; }
Foo.prototype.touch = function (this: any) { this.data.added = 1; };
const foo = new (Foo as any)();
foo.touch();

// 2 inherits via setPrototypeOf on prototypes
function Base(this: any) {}
Base.prototype.mark = function (this: any, o: any) { o.marked = 1; };
function Derived(this: any) {}
Object.setPrototypeOf(Derived.prototype, Base.prototype);
const dd = new (Derived as any)();
dd.mark({ inh1: 1 });

// 3 class extends an unmodeled builtin: unknown inherited methods escape args
class Buf extends Uint8Array { mine(o: any) { return o; } }
const b = new Buf(4);
(b as any).unknownInherited({ ub1: 1 });
b.mine({ ub2: 1 });

// 4 super calls and super props
class A { keep(o: any) { o.byA = 1; } }
class B extends A { keep(o: any) { super.keep(o); } }
new B().keep({ sup1: 1 });

// 5 mixin
function Mix<T extends new (...a: any[]) => any>(C: T) { return class extends C { mixed(o: any) { o.mx = 1; } }; }
class Plain {}
const M = Mix(Plain);
new M().mixed({ mix1: 1 });

// 6 new this.constructor
class Cloner { payload: any = null; clone() { const c = new (this.constructor as any)(); c.payload = { cl1: 1 }; return c; } }
const cc = new Cloner().clone();
cc.payload.more = 1;

// 7 for await over async generator
async function* agen() { yield { ag1: 1 }; }
(async () => { for await (const o of agen()) (o as any).k = 1; })();

// 8 Promise.all
(async () => { const [x] = await Promise.all([Promise.resolve({ pa1: 1 })]); (x as any).k = 1; })();

// 9 Array.from with mapper, sort comparator, filter chain
const made = Array.from([1, 2], (n) => ({ af: n }));
(made[0] as any).k = 1;
const sorted = [{ so1: 1 }].sort((p: any, q: any) => { p.cmp = 1; return 0; });
const filtered = [{ fi1: 1 }].filter((x) => true);
(filtered[0] as any).k = 1;

// 10 optional chaining and ??=
const maybe: any = { opt1: 1 };
maybe?.inner?.call?.(1);
const cache: { c?: any } = {};
const got = (cache.c ??= { lazy: 1 });
got.k = 1;

// 11 destructuring assignment
let da: any;
({ da } = { da: { dst1: 1 } });
da.k = 1;

// 12 static members and private methods
class S { static registry = { st1: 1 }; static add() { (S.registry as any).k = 1; } #priv(o: any) { o.pv = 1; } run() { this.#priv({ pr1: 1 }); } }
S.add();
new S().run();

// 13 Proxy escapes target and handler
const tgt = { px1: 1 };
new Proxy(tgt, {});

// 14 WeakRef deref
const wr = new WeakRef({ wr1: 1 });
(wr.deref() as any).k = 1;

// 15 structuredClone result is fresh: write is fine for the source
const src = { sc1: 1 };
const copy = structuredClone(src);
(copy as any).k = 1;

// 16 Object.entries pairs
for (const [k, v] of Object.entries({ e1: { ee1: 1 } })) (v as any).k = 1;

// 17 defineProperty getter this
const withGetter: any = {};
Object.defineProperty(withGetter, "g", { get() { return { gg1: 1 }; } });
withGetter.g.k = 1;

// 18 Reflect.apply now
function addKey(this: any) { this.added = 1; }
const viaApply = { a1: 1 };
Reflect.apply(addKey, viaApply, []);
