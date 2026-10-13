// @native count members lookups kinds caseOf viaGlobals grown
// @interpreted patchGet restoreGet patchParseInt restoreParseInt defineGlobal
// Direct paths: `Map.prototype.get`, `set`, `has` and `Set.prototype.add`,
// `has` called without a call frame while they are the builtins and the
// receiver is a Map or Set (a subclass's instance too; other receivers, a
// class with methods of those names, a replaced method take the call);
// strings keep their hash once a Map has computed it (and lose it where
// they grow in place); global builtins (`parseInt`, `String`) read through
// a cache while the global object keeps its shape (a replaced or added
// global is seen); `switch` and `===` on short string constants compare
// characters inline (16-bit strings, ropes and slices take the engine's
// comparison).

function count(words: string[]): string {
  const m = new Map<string, number>();
  for (const w of words) m.set(w, (m.get(w) ?? 0) + 1);
  return [...m.entries()].map(([k, v]) => k + "=" + v).join(",");
}

function members(xs: unknown[]): string {
  const s = new Set<unknown>();
  for (const x of xs) s.add(x);
  return xs.map((x) => (s.has(x) ? "y" : "n")).join("") + s.size;
}

class Box {
  items: string[] = [];
  get(i: number): string {
    return this.items[i] ?? "none";
  }
  set(i: number, v: string): Box {
    this.items[i] = v;
    return this;
  }
  has(v: string): boolean {
    return this.items.includes(v);
  }
  add(v: string): number {
    return this.items.push(v);
  }
}

class Counter extends Map<string, number> {
  bump(k: string): void {
    this.set(k, (this.get(k) ?? 0) + 10);
  }
}

function lookups(m: { get(k: unknown): unknown; has(k: unknown): boolean }, keys: unknown[]): string {
  return keys.map((k) => String(m.get(k)) + (m.has(k) ? "+" : "-")).join(" ");
}

// Keys of every kind, a key that grows in place between lookups.
function kinds(): string {
  const m = new Map<unknown, string>();
  const o = {};
  m.set(NaN, "nan").set(-0, "zero").set(o, "obj").set("ab", "s").set(1.5, "frac").set(true, "t");
  let k = "a";
  m.get(k);
  k += "b";
  const grown = m.get(k);
  return [m.get(NaN), m.get(0), m.get(o), m.get({}), grown, m.get(3 / 2), m.get(true), m.get("a")].join("|");
}

function caseOf(key: string): number {
  switch (key) {
    case "user":
      return 1;
    case "path":
      return 2;
    case "status":
      return 3;
    case "x":
      return 4;
    case "abcdefghijklmnop":
      return 5;
    case "seven77":
      return 6;
    case "héllo":
      return 7;
    default:
      return key === "bytes" ? 8 : 0;
  }
}

function viaGlobals(n: number): string {
  let t = 0;
  for (let i = 0; i < n; i++) t += parseInt(String(i * 7), 10) + Number("2");
  return String(t);
}

// A string that grows in place: its hash goes with its old value.
function grown(): string {
  const m = new Map<string, number>([["xy", 1], ["xyz", 2]]);
  let s = "x";
  for (let i = 0; i < 1; i++) s = s + String.fromCharCode(121);
  const a = m.get(s);
  s += "z";
  return a + ":" + m.get(s);
}

let savedGet: unknown;
function patchGet(): void {
  "use no aot";
  savedGet = Map.prototype.get;
  Map.prototype.get = function (this: Map<unknown, unknown>, k: unknown): unknown {
    return "patched:" + String(k);
  };
}
function restoreGet(): void {
  "use no aot";
  Map.prototype.get = savedGet as typeof Map.prototype.get;
}
let savedParseInt: unknown;
function patchParseInt(): void {
  "use no aot";
  savedParseInt = globalThis.parseInt;
  (globalThis as unknown as { parseInt: unknown }).parseInt = (s: string) => 100;
}
function restoreParseInt(): void {
  "use no aot";
  (globalThis as unknown as { parseInt: unknown }).parseInt = savedParseInt;
}
function defineGlobal(): void {
  "use no aot";
  Object.defineProperty(globalThis, "extraGlobal", { value: 1, configurable: true });
}

console.log(count(["a", "b", "a", "c", "b", "a", "a".repeat(40), "a".repeat(40)]));
console.log(members([1, "1", 1, NaN, NaN, -0, 0, null, undefined]));
const b = new Box();
b.set(0, "p").add("q");
console.log(b.get(0), b.get(5), b.has("q"), b.has("z"), b.add("r"));
const c = new Counter();
c.bump("k");
c.bump("k");
console.log(c.get("k"), c.has("k"), c instanceof Map);
const wm = new WeakMap<object, string>();
const key = {};
wm.set(key, "weak");
console.log(lookups(new Map<unknown, unknown>([["a", 1], [2, "two"]]), ["a", 2, "z"]), lookups(wm, [key, {}]));
console.log(kinds());
const wide = "userĀ".slice(0, 4);
console.log(
  ["user", "path", "status", "x", "abcdefghijklmnop", "seven77", "héllo", "bytes", "usex", "", wide, "ab".repeat(8)]
    .map(caseOf)
    .join(","),
  caseOf("abcdefgh" + "ijklmnop"),
  caseOf(("a".repeat(30) + "abcdefghijklmnop").slice(30)),
);
console.log(viaGlobals(50), grown());
patchGet();
console.log(count(["a"]), new Map([[1, 2]]).get(1));
restoreGet();
patchParseInt();
console.log(viaGlobals(3));
restoreParseInt();
defineGlobal();
console.log(viaGlobals(3));
try {
  console.log(Map.prototype.get.call(new Set(), 1));
} catch (e) {
  console.log((e as Error).constructor.name);
}
