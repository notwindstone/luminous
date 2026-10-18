// @native typed optionalFirst strings nested plainShapes dates replaced protoToJSON
// JSON.stringify by the writers generated for `aot type` layouts (called
// directly where the value is statically of one), by the per-shape key
// cache of plain objects, and of Dates: the same as the general
// algorithm (which a replacer or an indentation of 0 selects).
type int = number;

aot type Row = { id: number; name: string; x: number; y: int; on: boolean };
aot type Tail = { a: int } | { a: int; b: string } | { a: int; c: number };
aot type Head = { h: string } | { n: number } | { h: string; n: number; z: boolean };
aot type Outer = { tag: "p" | "q"; row: Row; label: string };

function same(label: string, fast: string, v: unknown): string {
  const general = JSON.stringify(v, null, 0);
  return label + " " + (fast === general ? "same" : "DIFF " + general) + " " + fast;
}

function typed(): string {
  const out: string[] = [];
  const rows: Row[] = [];
  for (let i: int = 0; i < 6; i++) {
    rows.push({ id: i * 0.5, name: "row" + i, x: i === 3 ? -0 : i * 1e21, y: (i * 1000003) | 0, on: i % 2 === 0 });
  }
  for (let i: int = 0; i < rows.length; i++) out.push(same("row" + i, JSON.stringify(rows[i]), rows[i]));
  const r: Row = rows[2];
  out.push(same("local", JSON.stringify(r), r));
  out.push(same("array", JSON.stringify(rows), rows));
  const missing = rows[10];
  out.push("missing " + String(JSON.stringify(missing)));
  const nums: Row = { id: NaN, name: "", x: Infinity, y: -2147483648, on: false };
  out.push(same("nums", JSON.stringify(nums), nums));
  return out.join("\n");
}

function optionalFirst(): string {
  const out: string[] = [];
  const hs: Head[] = [{ h: "only" }, { n: 1.25 }, { h: "all", n: 2, z: true }];
  for (let i: int = 0; i < hs.length; i++) out.push(same("head" + i, JSON.stringify(hs[i]), hs[i]));
  const ts: Tail[] = [{ a: 1 }, { a: 2, b: "bee" }, { a: 3, c: 0.75 }];
  for (let i: int = 0; i < ts.length; i++) out.push(same("tail" + i, JSON.stringify(ts[i]), ts[i]));
  return out.join("\n");
}

function strings(): string {
  const out: string[] = [];
  const names = ["", "a", "abc", "abcd", "abcdefgh", "sixteen-chars-xx", "seventeen-chars-x", "quo\"te", "back\\slash",
    "tab\there", "\u0001", "café", "日本", "😀", String.fromCharCode(0xd800), "x".repeat(300)];
  for (let i: int = 0; i < names.length; i++) {
    const r: Row = { id: i, name: names[i], x: 1, y: 2, on: true };
    out.push(same("name" + i, JSON.stringify(r), r));
  }
  return out.join("\n");
}

function nested(): string {
  const row: Row = { id: 7, name: "seven", x: 7.5, y: 7, on: true };
  const o: Outer = { tag: "q", row: row, label: "out\"er" };
  const list: Outer[] = [o, { tag: "p", row: row, label: "" }];
  return [same("outer", JSON.stringify(o), o), same("list", JSON.stringify(list), list),
    same("mixed", JSON.stringify({ list: list, n: 1 }), { list: list, n: 1 })].join("\n");
}

function plainShapes(): string {
  const out: string[] = [];
  const objs: any[] = [];
  for (let i: int = 0; i < 4; i++) objs.push({ id: i, name: "n" + i, ok: i > 1, none: null, x: i / 3 });
  for (const o of objs) out.push(same("plain", JSON.stringify(o), o));
  objs[1].extra = [1, "two", { three: 3 }];
  delete objs[2].name;
  Object.defineProperty(objs[3], "id", { enumerable: false });
  objs[0].fn = () => 1;
  for (const o of objs) out.push(same("changed", JSON.stringify(o), o));
  const keyed: any = { "a key longer than sixteen": 1, "q\"k": 2, "é": 3, short: "s" };
  out.push(same("keys", JSON.stringify(keyed), keyed));
  return out.join("\n");
}

function dates(): string {
  const out: string[] = [];
  const ds = [new Date(0), new Date(-1), new Date(951782400000), new Date(-62167219200001), new Date(253402300800000),
    new Date(8.64e15), new Date(NaN)];
  for (const d of ds) out.push(same("date", JSON.stringify({ d: d, list: [d] }), { d: d, list: [d] }));
  const saved = Date.prototype.toJSON;
  Date.prototype.toJSON = function () {
    return "own";
  };
  out.push(same("toJSON", JSON.stringify({ d: ds[0] }), { d: ds[0] }));
  Date.prototype.toJSON = saved;
  out.push(same("restored", JSON.stringify({ d: ds[0] }), { d: ds[0] }));
  return out.join("\n");
}

function replaced(): string {
  const r: Row = { id: 1, name: "r", x: 2, y: 3, on: false };
  const saved = JSON.stringify;
  (JSON as any).stringify = (v: unknown) => "replaced " + typeof v;
  const a = JSON.stringify(r);
  (JSON as any).stringify = saved;
  return a + " / " + JSON.stringify(r);
}

function protoToJSON(): string {
  const r: Row = { id: 1, name: "r", x: 2, y: 3, on: false };
  (Object.prototype as any).toJSON = function () {
    return "proto";
  };
  const a = JSON.stringify(r) + " " + JSON.stringify({ k: 1 }) + " " + JSON.stringify([r]);
  delete (Object.prototype as any).toJSON;
  return a + " / " + JSON.stringify(r);
}

console.log(typed());
console.log(optionalFirst());
console.log(strings());
console.log(nested());
console.log(plainShapes());
console.log(dates());
console.log(replaced());
console.log(protoToJSON());
