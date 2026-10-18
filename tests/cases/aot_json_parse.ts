// @native errorOf parseUser parseUsers total asCast deep viaMethod fetched
// `JSON.parse(text)` and `await r.json()` where an `aot type` is expected:
// the result converted to it (only its fields kept, each checked), which
// native code parses to directly; the same objects and errors either way.
type int = number;

aot type Point = { x: number; y: number };
aot type User = {
  id: int;
  name: string;
  email: string | null;
  age?: int;
  role: "admin" | "user";
  tags: { t: string }[];
  home: Point;
  ok: boolean;
};
aot type Tree = { label: string; children: Tree[] };
aot type Shape = { kind: "circle"; r: number } | { kind: "square"; side: number };

function errorOf(f: () => void): string {
  try {
    f();
    return "ok";
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

function parseUser(text: string): User {
  return JSON.parse(text);
}

function parseUsers(text: string): User[] {
  const us: User[] = JSON.parse(text);
  return us;
}

function total(us: User[]): number {
  let t = 0;
  for (let i: int = 0; i < us.length; i++) t += us[i].id + us[i].home.x + us[i].tags.length;
  return t;
}

function asCast(text: string): string {
  const s = JSON.parse(text) as Shape;
  return s.kind === "circle" ? "circle " + s.r : "square " + s.side;
}

function deep(t: Tree): int {
  let d: int = 0;
  for (let i: int = 0; i < t.children.length; i++) {
    const c: int = deep(t.children[i]) | 0;
    if (c > d) d = c;
  }
  return (d + 1) | 0;
}

const base = {
  id: 7,
  name: "Ann",
  email: null,
  role: "admin",
  tags: [{ t: "a", extra: 1 }, { t: "b" }],
  home: { x: 1.5, y: -2, z: 9 },
  ok: true,
  unknown: { nested: [1, 2, { x: "y" }], s: "é\"\\" },
};
const text = JSON.stringify(base);
const u = parseUser(text);
console.log(JSON.stringify(u), Object.keys(u).join(","), "unknown" in u, Object.isExtensible(u));
console.log(u.id + 1, u.home.x, u.tags[1].t, u.email, u.age);

const variants = [
  `{"id":1,"name":"café","email":"a@b","age":30,"role":"user","tags":[],"home":{"x":0,"y":0},"ok":false}`,
  `{"id":2,"name":"\\u0041\\n\\ud83d\\ude00","email":null,"role":"user","tags":[],"home":{"x":1e3,"y":-0},"ok":true}`,
  `{"id":3,"name":"emoji 😀","email":null,"role":"user","tags":[],"home":{"x":0.1,"y":2},"ok":true}`,
  `{"id":2147483648,"name":"wrap","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  `{"id":-0,"name":"neg","email":null,"role":"user","tags":[],"home":{"x":-0,"y":2},"ok":true}`,
  `{"id":4,"id":5,"name":"dup","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  `{"\\u0069d":6,"name":"escaped key","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  ` \n {"id":8,"name":"ws","email":null,"role":"user","tags":[ {"t":"x"} ],"home":{"x":1,"y":2},"ok":true} \t`,
];
for (const v of variants) {
  console.log(errorOf(() => {
    const p = parseUser(v);
    console.log(JSON.stringify(p), Object.is(p.home.y, -0), Object.is(p.id, -0), p.name.length);
  }));
}

const bad = [
  `{"id":1}`,
  `{"id":"1","name":"x","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  `{"id":1.5,"name":"x","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  `{"id":1,"name":"x","email":null,"role":"guest","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  `{"id":1,"name":"x","email":5,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  `{"id":1,"name":"x","email":null,"age":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
  `{"id":1,"name":"x","email":null,"role":"user","tags":[null],"home":{"x":1,"y":2},"ok":true}`,
  `{"id":1,"name":"x","email":null,"role":"user","tags":{},"home":{"x":1,"y":2},"ok":true}`,
  `{"id":1,"name":"x","email":null,"role":"user","tags":[],"home":null,"ok":true}`,
  `{"id":1,"name":"x","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":1}`,
  `{"id":1,"name":"x",}`,
  `{"id":01}`,
  `[1, 2]`,
  `"text"`,
  `null`,
  ``,
  `{"id":1,"name":"x","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true} x`,
  `{"id":1,"name":"\u0001","email":null,"role":"user","tags":[],"home":{"x":1,"y":2},"ok":true}`,
];
for (const b of bad) console.log(errorOf(() => parseUser(b)));

const list = JSON.stringify([base, { ...base, id: 8, tags: [] }, { ...base, id: 9, home: { x: 3, y: 4 } }]);
const us = parseUsers(list);
console.log(us.length, total(us), Array.isArray(us), JSON.stringify(us.map((x) => x.id)));
console.log(errorOf(() => parseUsers(`{"id":1}`)), errorOf(() => parseUsers(`[{"id":1}]`)));

console.log(asCast(`{"kind":"circle","r":2,"side":9}`), asCast(`{"kind":"square","side":3}`));
console.log(errorOf(() => asCast(`{"kind":"square","r":3}`)), errorOf(() => asCast(`{"kind":"triangle"}`)));

const tree: Tree = JSON.parse(
  `{"label":"root","children":[{"label":"a","children":[{"label":"a1","children":[],"x":1}]},{"label":"b","children":[]}]}`,
);
console.log(deep(tree), JSON.stringify(tree));

// A replaced JSON.parse: its result converted.
const saved = JSON.parse;
(JSON as any).parse = (t: string) => ({ ...saved(t), name: "replaced", extra: true });
console.log(JSON.stringify(parseUser(text)));
(JSON as any).parse = saved;
console.log(parseUser(text).name);

// `await r.json()`: any `r` (here not a host body: its json() then converted).
function viaMethod(o: { json(): Promise<unknown> }): Promise<User> {
  return (async () => {
    const r: User = await o.json();
    return r;
  })();
}
async function fetched(): Promise<string> {
  const a = await viaMethod({ json: () => Promise.resolve(JSON.parse(text)) });
  const b = (await ({ json: async () => ({ x: 1, y: 2, w: 3 }) }).json()) as Point;
  let err = "";
  try {
    const c: Point = await { json: async () => ({ x: "no" }) }.json();
    err = "no error " + JSON.stringify(c);
  } catch (e) {
    err = (e as Error).message;
  }
  return JSON.stringify(a) + " " + JSON.stringify(b) + " " + err;
}
fetched().then((s) => console.log(s));
