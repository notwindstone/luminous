// stable: read, written at own keys, passed around
function makePoint(x: number, y: number) { return { x, y }; }
function len(p: { x: number; y: number }) { return Math.sqrt(p.x * p.x + p.y * p.y); }
const pts = [makePoint(1, 2), makePoint(3, 4)];
for (const p of pts) { p.x += 1; }
console.log(pts.map(len));

// adds a key later
const opt: { a: number; b?: number } = { a: 1 };
opt.b = 2;

// computed write
const rec: Record<string, number> = { first: 1 };
function setKey(o: Record<string, number>, k: string) { o[k] = 3; }
setKey(rec, "x" + Math.random());

// delete
const d = { gone: 1, kept: 2 };
delete (d as any).gone;

// the user's examples
const GlobalObject = { someBigObj: { inner: 1 }, anotherProp: { other: 2 } } as const;
const WriteableObject: { hello: Array<number> } = { hello: [] };
function run() { return <Key extends keyof typeof WriteableObject>(key: Key) => WriteableObject[key].push(1); }
run()("hello");

const { someBigObj: hmmm } = GlobalObject;
(hmmm as any).ouch = "layout breaking";

declare const window: any;
const escapes = { e: 1 };
(globalThis as any).huh = escapes;

const viaFn = { v: 1 };
((async function () {}).constructor("return 1"))(viaFn);

const dyn: any = {};
const target = { t: 1 };
dyn.hehe = target;
dyn["he" + "he"].trolled = true;

// object spread, assign, freeze
const base = { a: 1, b: 2 };
const merged = { ...base, c: 3 };
const assigned = { z: 1 };
Object.assign(assigned, { y: 2 });
const frozen = Object.freeze({ f: 1 });

// class field holding a literal, method writes it
class Box { inner = { n: 0 }; bump() { this.inner.n++; } grow() { (this.inner as any).m = 1; } }
const bx = new Box(); bx.bump(); bx.grow();

// promise and await
async function later() { return { late: 1 }; }
later().then((o) => { (o as any).added = 2; });

// Map values
const m = new Map<string, { v: number }>();
m.set("a", { v: 1 });
m.get("a")!.v = 2;

const m2 = new Map<string, { w: number }>();
m2.set("b", { w: 1 });
for (const [, val] of m2) (val as any).extra = 1;

// string split then map to objects
const parts = "a,b".split(",").map((s) => ({ s }));
(parts[0] as any).q = 1;

// JSON
const fromJson = JSON.parse("{}");
fromJson.attached = { j: 1 };
