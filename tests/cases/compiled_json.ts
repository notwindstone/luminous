// @native shapes numbers strings arrays fallbacks
// JSON.stringify of `aot type` objects and arrays: written from their
// fields (JS_SetJSONWriter), the same as the general algorithm writes them
// (which a replacer or indentation of 0 selects).

aot type Inner = { x: int; y: number };

aot type Rec = {
  id: int;
  name: string;
  ratio: number;
  on: boolean;
  inner: Inner;
  kind: "a" | "b";
};

aot type Opt = { a: number } | { b: string } | { a: number; c: Inner };

aot type Keys = { "content-type": string; "quo\"te": int; "back\\slash": int; café: boolean; "tab\tkey": int; "0": int };

aot type Holder = { items: string; inner: Inner };

function errorOf(f: () => void): string {
  try {
    f();
    return "ok";
  } catch (e) {
    return (e as Error).constructor.name;
  }
}

function same(label: string, v: unknown): string {
  const fast = JSON.stringify(v);
  const general = JSON.stringify(v, null, 0);
  const replaced = JSON.stringify(v, (_k, x) => x);
  return label + " " + (fast === general && fast === replaced ? "same" : "DIFF " + general) + " " + fast;
}

function shapes(): string {
  const out: string[] = [];
  const r: Rec = { id: 7, name: "Ada", ratio: 0.25, on: true, inner: { x: -3, y: 1.5 }, kind: "a" };
  out.push(same("rec", r));
  r.on = false;
  r.kind = "b";
  r.inner = { x: 2147483647, y: -0 };
  out.push(same("rec2", r));
  const opts: Opt[] = [{ a: 1 }, { b: "two" }, { a: 3, c: { x: 1, y: 2 } }];
  for (const o of opts) out.push(same("opt", o));
  out.push(same("opts", opts));
  const k: Keys = { "content-type": "text/plain", "quo\"te": 1, "back\\slash": 2, café: true, "tab\tkey": 3, "0": 4 };
  out.push(same("keys", k), Object.keys(k).join("|"));
  return out.join("\n");
}

function numbers(): string {
  const out: string[] = [];
  const ys = [0, -0, 1, -1, 0.1, 1e21, 1e-7, 2 ** 31, -(2 ** 31), 2 ** 53, NaN, Infinity, -Infinity, 123456.789];
  for (const y of ys) {
    const p: Inner = { x: 0, y };
    out.push(same("y", p));
  }
  const q: Inner = { x: -2147483648, y: 5e-324 };
  out.push(same("min", q));
  return out.join("\n");
}

function strings(): string {
  const out: string[] = [];
  let long = "";
  for (let i = 0; i < 100; i++) long = long + "0123456789";
  const vals = ["", "plain", 'q"uote', "back\\slash", "nl\nline", "\u0001ctl", "café", "日本", "emoji \u{1F600}", "lone " + String.fromCharCode(0xd800), long];
  for (const s of vals) {
    const h: Holder = { items: s, inner: { x: s.length, y: 0 } };
    const j = JSON.stringify(h);
    out.push(same("str", h).slice(0, 80), String(j.length), String(JSON.parse(j).items === s));
  }
  return out.join("\n");
}

function arrays(): string {
  const out: string[] = [];
  const a: Inner[] = new Array(4);
  a[1] = { x: 1, y: 1.5 };
  a[3] = { x: 3, y: 3 };
  out.push(same("holes", a));
  const b: Inner[] = [];
  out.push(same("empty", b));
  for (let i = 0; i < 5; i++) b.push({ x: i, y: i / 4 });
  out.push(same("pushed", b));
  out.push(same("in plain", { list: b, n: b.length, nested: [a, b] }));
  out.push(same("plain array", [b[0], b[1], null, "s"]));
  return out.join("\n");
}

function fallbacks(): string {
  const out: string[] = [];
  const p: Inner = { x: 1, y: 2 };
  const a: Inner[] = [p, { x: 3, y: 4 }];
  // toJSON on Object.prototype or Array.prototype: the general algorithm.
  (Object.prototype as Record<string, unknown>).toJSON = function () {
    return "O";
  };
  out.push(JSON.stringify(p), JSON.stringify(a));
  delete (Object.prototype as Record<string, unknown>).toJSON;
  (Array.prototype as unknown as Record<string, unknown>).toJSON = function () {
    return "A";
  };
  out.push(JSON.stringify(p), JSON.stringify(a));
  delete (Array.prototype as unknown as Record<string, unknown>).toJSON;
  out.push(same("restored", a));
  // Another prototype for the array (its objects' is fixed).
  const b: Inner[] = [{ x: 5, y: 6 }];
  out.push(String(Object.isExtensible(p)), errorOf(() => Object.setPrototypeOf(p, null)));
  Object.setPrototypeOf(b, { toJSON: () => "B" });
  out.push(JSON.stringify(b), JSON.stringify({ b }));
  Object.setPrototypeOf(b, null);
  out.push(same("null proto", b), same("in array", [b, a]));
  // Indentation and a key list.
  out.push(JSON.stringify(a, null, 1), JSON.stringify(p, ["y"]));
  // Deep nesting: past the fast path's depth, the general algorithm.
  let deep: unknown = p;
  for (let i = 0; i < 40; i++) deep = [deep];
  out.push(same("deep", deep).slice(0, 9));
  return out.join("\n");
}

console.log(shapes());
console.log(numbers());
console.log(strings());
console.log(arrays());
console.log(fallbacks());
