// @native parse same errorOf
// JSON.parse's fast path (the text's characters in place, objects made with
// the shapes of the last ones): the same values, keys in the same order, and
// the same errors as the general parser (which a reviver makes run).
function parse(t: string): any {
  return JSON.parse(t);
}

function same(a: any, b: any, path: string): string {
  if (typeof a !== typeof b) return path + ": " + typeof a + " vs " + typeof b;
  if (a === null || typeof a !== "object") return Object.is(a, b) ? "" : path + ": " + String(a) + " vs " + String(b);
  if (Array.isArray(a) !== Array.isArray(b)) return path + ": array";
  const ka = Object.keys(a), kb = Object.keys(b);
  if (ka.join("\u0001") !== kb.join("\u0001")) return path + ": keys " + ka.join() + " | " + kb.join();
  for (const k of ka) {
    const r = same(a[k], b[k], path + "." + k);
    if (r) return r;
  }
  return "";
}

function errorOf(t: string): string {
  try {
    parse(t);
    return "ok";
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

const cases = [
  `{"id":1,"name":"user1","email":"u1@example.com","active":true,"score":12.5,"home":{"x":1,"y":2}}`,
  `[{"a":1,"b":2},{"a":3,"b":4},{"a":5},{"b":6,"a":7},{"a":8,"b":9,"c":10},{"a":1,"a":2,"b":3}]`,
  `{"__proto__":{"x":1},"y":2}`, `{"1":1,"0":0,"b":2,"a":3,"4294967295":5,"01":6,"-1":7}`,
  `{"a":{"a":{"a":{"a":[1,[2,[3,[4]]]]}}}}`, `{"\\u0061":1,"a":2}`, `{"é":1,"\\u00e9":2,"ü😀":3,"\\ud83d\\ude00":4}`,
  `"esc\\n\\t\\"\\\\\\/\\b\\f\\r"`, `"\\u0041\\u00ff\\u0100\\ud800"`, `"latin é ü ß"`, `"emoji 😀 ok"`,
  `0`, `-0`, `-0.0`, `1`, `2147483647`, `2147483648`, `-2147483648`, `-2147483649`, `1.5`, `1e3`, `1E-3`, `0.1`,
  `123456789012345678901234567890`, `1.7976931348623157e308`, `1e309`, `-1e-400`, `5e-324`, `9007199254740993`,
  `true`, `false`, `null`, ` \n\t\r [ 1 , 2 ] \n `, `[]`, `{}`, `[{}]`, `[[]]`, `{"":1,"":2}`,
  `[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35]`,
  `{"a":"0123456789abcdef0123","b":"0123456789\\n0123456789","c":"0123456789é0123"}`,
];
let diffs = 0;
for (const t of cases) {
  const r = same(parse(t), JSON.parse(t, (k, v) => v), "$");
  if (r) {
    diffs++;
    console.log("differs:", t, r);
  }
}
// Objects of the same keys again and again (the shapes cached), others in between.
for (let i = 0; i < 50; i++) {
  const t = i % 3 ? `{"id":${i},"name":"n${i}","tags":["a","b"]}` : `{"id":${i},"tags":[],"name":"m"}`;
  const r = same(parse(t), JSON.parse(t, (k, v) => v), "$");
  if (r) diffs++;
}
console.log("cases", cases.length, "differ", diffs);
console.log(JSON.stringify(parse(cases[0])), JSON.stringify(parse(cases[1])), Object.keys(parse(cases[3])).join());
const p = parse(`{"__proto__":{"x":1},"y":2}`);
console.log(Object.getPrototypeOf(p) === Object.prototype, JSON.stringify(Object.getOwnPropertyDescriptor(p, "__proto__")));
const o = parse(`{"a":1,"b":2}`);
o.c = 3;
delete o.a;
console.log(JSON.stringify(o), JSON.stringify(parse(`{"a":1,"b":2}`)), Object.isFrozen(Object.freeze(parse(`{"a":1}`))));
for (const t of [`{"a":1,}`, `[1,]`, `{"a" 1}`, `{a:1}`, `[01]`, `[1.]`, `[.5]`, `[+1]`, `[-]`, `[1e]`, `"\\x"`, `"\\u12"`,
  `"a\u0001b"`, `[1] x`, ``, ` `, `nul`, `truex`, `{"a":1}}`, `"unterminated`, `[1,2`, `{"a":`, `'s'`, `[NaN]`]) {
  console.log(JSON.stringify(t), errorOf(t));
}
