// @native get set bump each many
// Computed member reads and writes with symbol keys through a cache of
// the site (the key's own data property, a prototype's data property or
// getter): shapes that change, keys that change at a site, setters,
// read-only properties, proxies and integer keys there.
const kA = Symbol("a"), kB = Symbol("b"), kG = Symbol("g");
function get(o: any, k: any): any { return o[k]; }
function set(o: any, k: any, v: any): void { o[k] = v; }
function bump(o: any): number { return o[kA]++; }
function each(xs: any[], k: any): string { let s = ""; for (const x of xs) s += String(x[k]) + ","; return s; }
function many(o: any): string { const ks = [kA, kB, kG, "s", 0]; return ks.map((k) => String(o[k])).join(","); }
class P { get [kG]() { return "getter:" + (this as any)[kA]; } set [kG](v: any) { (this as any)[kB] = "set " + v; } }
const o: any = new P();
o[kA] = 1;
o[kB] = 2;
const out: string[] = [];
for (let i = 0; i < 5; i++) { set(o, kA, i); bump(o); }
out.push([get(o, kA), get(o, kB), get(o, kG)].join(" "));
set(o, kG, "x");
out.push(get(o, kB));
// Another shape at the same sites, then a key deleted, then re-added.
const q: any = { [kB]: "q" };
q[kA] = 10;
out.push(each([o, q, o, q], kA) + each([o, q], kB));
delete o[kA];
out.push(String(get(o, kA)) + " " + get(o, kG));
o[kA] = 7;
out.push(String(get(o, kA)) + " " + bump(o) + " " + o[kA]);
// A read-only property: the write throws (strict module code).
const ro: any = Object.defineProperty({}, kA, { value: 1, writable: false });
try { set(ro, kA, 2); out.push("no throw"); } catch (e) { out.push((e as Error).constructor.name + " " + ro[kA]); }
// A frozen object, a proxy, a prototype's data property, Symbol.iterator.
const fr: any = Object.freeze({ [kA]: "frozen" });
const px: any = new Proxy({}, { get: (_t, k) => "proxy " + String(k), set: (_t, k, v) => { out.push("trap " + String(k) + "=" + v); return true; } });
const child: any = Object.create({ [kA]: "inherited" });
out.push([get(fr, kA), get(px, kA), get(child, kA), typeof get([], Symbol.iterator)].join(" | "));
set(px, kA, 5);
// Many keys at one site (it stops caching), integers and strings too.
out.push(many(Object.assign({ [kA]: "A", [kB]: "B", s: "S", 0: "zero" }, {})) + " " + many(o));
for (let i = 0; i < 3; i++) { set(child, kA, "own" + i); set(child, i, i * 2); }
out.push(get(child, kA) + " " + get(child, 2) + " " + get(Object.getPrototypeOf(child), kA));
console.log(out.join("\n"));
