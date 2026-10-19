// @native make makeWith addThree addMany dict readAll errorOf setterAppears frozenAdd
// Object.create(p) made inline while it is the builtin (the empty shape
// of p cached by the site), property additions through a site's cached
// transition, and dictionaries with dynamic keys: the same objects and
// errors as the engine's general paths.
function errorOf(f: () => unknown): string {
  try {
    return "ok " + JSON.stringify(f());
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

function make(p: any): any {
  return Object.create(p);
}

function makeWith(p: any, props: any): any {
  return Object.create(p, props);
}

function addThree(o: any, i: number): any {
  o.a = i;
  o.b = "b" + i;
  o.c = i * 2;
  return o;
}

function addMany(o: any): any {
  o.p1 = 1; o.p2 = 2; o.p3 = 3; o.p4 = 4; o.p5 = 5; o.p6 = 6; o.p7 = 7; o.p8 = 8; o.p9 = 9;
  return o;
}

function dict(keys: string[], d: any): number {
  for (let i = 0; i < keys.length; i++) d[keys[i]] = i;
  let s = 0;
  for (let i = 0; i < keys.length; i++) s += d[keys[i]] === undefined ? 1000 : d[keys[i]];
  return s;
}

function readAll(d: any, keys: string[]): string {
  return keys.map((k) => String(d[k])).join(",");
}

function setterAppears(): string {
  const out: string[] = [];
  for (let round = 0; round < 3; round++) {
    if (round === 1) {
      Object.defineProperty(Object.prototype, "b", { set(v) { out.push("setter " + v); }, configurable: true });
    }
    if (round === 2) delete (Object.prototype as any).b;
    const o = addThree({}, round);
    out.push(JSON.stringify(o) + " own b: " + Object.prototype.hasOwnProperty.call(o, "b"));
  }
  return out.join(" | ");
}

function frozenAdd(): string {
  return [
    errorOf(() => addThree(Object.preventExtensions({}), 1)),
    errorOf(() => addThree(Object.freeze({}), 1)),
    errorOf(() => addThree(Object.seal({ a: 0 }), 1)),
    errorOf(() => addThree(Object.defineProperty({}, "b", { value: 0, writable: false }), 1)),
  ].join(" | ");
}

// Object.create
const proto = { greet(): string { return "hi " + (this as any).name; } };
const nul = make(null);
console.log(Object.getPrototypeOf(nul) === null, "toString" in nul, JSON.stringify(nul), Object.keys(nul).length);
nul.x = 1;
nul["__proto__"] = 5;
console.log(JSON.stringify(nul), Object.keys(nul).join(), Object.getPrototypeOf(nul) === null);
for (let i = 0; i < 3; i++) {
  const o = make(i === 1 ? proto : null);
  o.name = "n" + i;
  console.log(Object.getPrototypeOf(o) === proto, typeof o.greet === "function" ? o.greet() : "none", JSON.stringify(o));
}
const arrProto = make(Array.prototype);
console.log(Array.isArray(arrProto), typeof arrProto.push, Object.getPrototypeOf(arrProto) === Array.prototype);
console.log(errorOf(() => make(1)), errorOf(() => make(undefined)), errorOf(() => make("s")));
console.log(JSON.stringify(Object.getOwnPropertyDescriptor(makeWith(null, { k: { value: 1, enumerable: true } }), "k")));
const saved = Object.create;
(Object as any).create = (p: any) => ({ replaced: true, p: p === null ? "null" : typeof p });
console.log(JSON.stringify(make(null)), JSON.stringify(make(proto)));
(Object as any).create = saved;
console.log(Object.getPrototypeOf(make(null)) === null);

// Additions
const objs: any[] = [];
for (let i = 0; i < 5; i++) objs.push(addThree(i % 2 ? Object.create(null) : {}, i));
console.log(objs.map((o) => JSON.stringify(o)).join(" "), Object.getPrototypeOf(objs[1]) === null);
console.log(JSON.stringify(addMany({})), JSON.stringify(addMany(Object.create(null))), JSON.stringify(addMany({ z: 0 })));
const big = addMany(addMany({}));
console.log(Object.keys(big).length, big.p9);
console.log(setterAppears());
console.log(frozenAdd());
const withGetter = Object.defineProperty({}, "a", { get() { return 42; }, configurable: true });
console.log(errorOf(() => addThree(withGetter, 1)));

// Dictionaries with dynamic keys
const keys: string[] = [];
for (let i = 0; i < 40; i++) keys.push("key" + i);
keys.push("__proto__", "toString", "1", "01", "constructor", "");
const d1: any = {};
const d2: any = Object.create(null);
console.log(dict(keys, d1), dict(keys, d2), Object.keys(d1).length, Object.keys(d2).length);
console.log(Object.getPrototypeOf(d1) === Object.prototype, typeof d1.toString, Object.getPrototypeOf(d2) === null);
console.log(readAll(d1, ["key0", "key39", "1", "01", "", "missing"]), readAll(d2, ["__proto__", "toString", "constructor"]));
const ro: any = Object.defineProperty({}, "key0", { value: "fixed", writable: false, enumerable: true });
console.log(errorOf(() => dict(["key0", "key1"], ro)), JSON.stringify(ro));
const getterDict: any = { get key1() { return "g"; } };
console.log(readAll(getterDict, ["key1", "key2"]));
const inherited: any = Object.create({ key3: "inh" });
console.log(readAll(inherited, ["key3"]), dict(["key3"], inherited), Object.keys(inherited).join());
