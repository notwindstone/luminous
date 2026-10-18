// @native names total deepest rename append checked
// Fields of an `aot type` that are object types written in place (each a
// layout of its own, `C.nested`) and arrays of objects of an `aot type`
// (`T[]`, `Array<T>`, or an `aot type` naming one): made by literals,
// typed in native code, kept to their types.
type int = number;

aot type C = { nested: { notNested: string } };
aot type D = C[];
aot type Leaf = { b: string; n: int };
aot type E = {
  heh: boolean;
  items: D;
  more: Array<C>;
  inline: { a: int; deep: Leaf[]; pair: { x: number } | { y: string } }[];
};
aot type Tree = { label: string; children: Tree[] };

function errorOf(f: () => void): string {
  try {
    f();
    return "ok";
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

function names(d: D): string {
  let s = "";
  for (let i: int = 0; i < d.length; i++) s += d[i].nested.notNested + ";";
  return s;
}

function total(e: E): int {
  let t: int = 0;
  for (let i: int = 0; i < e.inline.length; i++) {
    const row = e.inline[i];
    t = (t + row.a) | 0;
    for (let j: int = 0; j < row.deep.length; j++) t = (t + row.deep[j].n) | 0;
  }
  return t;
}

function deepest(t: Tree): int {
  let d: int = 0;
  for (let i: int = 0; i < t.children.length; i++) {
    const c: int = deepest(t.children[i]) | 0;
    if (c > d) d = c;
  }
  return (d + 1) | 0;
}

function rename(e: E, to: string): void {
  for (let i: int = 0; i < e.items.length; i++) e.items[i].nested.notNested = to + i;
  e.more[0].nested = { notNested: "replaced" };
}

function append(e: E, n: int): void {
  for (let i: int = 0; i < n; i++) {
    e.inline.push({ a: i, deep: [{ b: "d" + i, n: i * 10 }], pair: { x: i / 2 } });
  }
  e.inline[0].deep.push({ b: "extra", n: 1000 });
}

function checked(e: E): string {
  const out: string[] = [];
  out.push(errorOf(() => ((e as any).items = [{ nested: { notNested: "plain array" } }])));
  out.push(errorOf(() => ((e as any).items = e.more)));
  out.push(errorOf(() => (e.items as any).push({ nested: 1 })));
  out.push(errorOf(() => ((e as any).inline = e.items)));
  out.push(errorOf(() => ((e.items[0] as any).nested = { notNested: "plain object" })));
  out.push(errorOf(() => ((e.items[0].nested as any).notNested = 5)));
  out.push(errorOf(() => ((e.inline[0] as any).pair = { z: 1 })));
  out.push(errorOf(() => ((e.inline[0].pair as any).y = 5)));
  out.push(errorOf(() => (e.items = [{ nested: { notNested: "assigned" } }])));
  return out.join("\n");
}

const c: C = { nested: { notNested: "x" } };
const e: E = {
  heh: true,
  items: [c, { nested: { notNested: "y" } }],
  more: [],
  inline: [{ a: 1, deep: [{ b: "q", n: 2 }], pair: { y: "why" } }],
};
e.more.push({ nested: { notNested: "pushed" } });
console.log(JSON.stringify(e));
console.log(names(e.items), names(e.more), total(e));
append(e, 3);
rename(e, "r");
console.log(names(e.items), names(e.more), total(e));
console.log(Array.isArray(e.items), e.items.length, Object.keys(e.inline[0]).join(","), Object.isExtensible(c.nested));
console.log(checked(e));
console.log(JSON.stringify(e));
const t: Tree = { label: "root", children: [{ label: "a", children: [] }, { label: "b", children: [{ label: "c", children: [] }] }] };
t.children[0].children.push({ label: "a1", children: [{ label: "a2", children: [] }] });
console.log(deepest(t), JSON.stringify(t));
