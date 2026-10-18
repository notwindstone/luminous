// @native make keysOf isArr str
// Static methods of builtin constructors and namespaces, whose shapes the
// runtime shares at startup so that sites cache them: replaced, deleted,
// added to, and their constructors' prototypes changed after the sites
// cached them.
function make(p: any): any { return Object.create(p); }
function keysOf(o: any): string { return Object.keys(o).join(); }
function isArr(x: any): boolean { return Array.isArray(x); }
function str(x: any): string { try { return JSON.stringify(x); } catch (e) { return "threw " + (e as Error).constructor.name; } }
const out: string[] = [];
const proto = { m: 1 };
for (let i = 0; i < 5; i++) { make(proto); keysOf({ a: i }); isArr([i]); str({ i }); }
out.push(String(make(proto).m) + " " + keysOf({ a: 1, b: 2 }) + " " + isArr([]) + " " + isArr({}) + " " + str([1]));
const create = Object.create, keys = Object.keys, isArray = Array.isArray, stringify = JSON.stringify;
(Object as any).create = (p: any) => ({ patched: p === proto });
(Object as any).keys = () => ["patched"];
(Array as any).isArray = () => "patched";
(JSON as any).stringify = () => "patched";
out.push(JSON.stringify(make(proto)) + " " + keysOf({ a: 1 }) + " " + isArr([]) + " " + str(1));
(Object as any).create = create; (Object as any).keys = keys; (Array as any).isArray = isArray; (JSON as any).stringify = stringify;
out.push(String(make(proto).m) + " " + keysOf({ c: 3 }) + " " + isArr([]) + " " + str([2]));
delete (JSON as any).stringify;
out.push(str(3));
(JSON as any).stringify = stringify;
// Added statics, a getter, and a prototype for the namespace.
(Object as any).extra = 1;
Object.defineProperty(Array, "isArray", { get() { return () => "getter"; }, configurable: true });
out.push(isArr([]) + " " + keysOf({ d: 4 }));
Object.defineProperty(Array, "isArray", { value: isArray, writable: true, configurable: true });
Object.setPrototypeOf(JSON, { stringify: () => "inherited" });
delete (JSON as any).stringify;
out.push(str(5) + " " + isArr([]));
console.log(out.join("\n"));
