// @native compare names undeclared main
// @interpreted kinds
// `typeof v === "name"` tests v's tag (and an object's class) without making
// the string; `typeof v` alone gives the name's constant. Every kind of
// value, compared both ways round, strictly, loosely and negated.
class Point {
  x = 1;
}
function* gen() {
  yield 1;
}
async function af() {
  return 1;
}
async function* agen() {
  yield 1;
}

function kinds(): any[] {
  const big = 2n ** 100n;
  let rope = "a".repeat(40);
  rope = rope + rope + "b";
  return [
    undefined,
    null,
    true,
    false,
    0,
    -1.5,
    NaN,
    "",
    "str",
    rope,
    Symbol("s"),
    1n,
    big,
    {},
    [],
    new Uint8Array(4),
    new Map(),
    /re/,
    new Date(0),
    new Point(),
    Point,
    () => 1,
    function () {},
    gen,
    gen(),
    af,
    agen,
    Math.max,
    Math.max.bind(null),
    Promise.resolve,
    new Proxy({}, {}),
    new Proxy(() => 1, {}),
    new Proxy(class {}, {}),
    Object.create(null),
    new Error("e"),
    new Boolean(false),
  ];
}

function compare(v: any): string {
  let s = "";
  s += typeof v === "undefined" ? "u" : "-";
  s += typeof v === "object" ? "o" : "-";
  s += typeof v === "boolean" ? "b" : "-";
  s += typeof v === "number" ? "n" : "-";
  s += typeof v === "string" ? "s" : "-";
  s += typeof v === "symbol" ? "y" : "-";
  s += typeof v === "bigint" ? "i" : "-";
  s += typeof v === "function" ? "f" : "-";
  s += "function" === typeof v ? "F" : "-";
  s += typeof v == "object" ? "O" : "-";
  s += typeof v !== "string" ? "!" : "-";
  s += typeof v != "number" ? "#" : "-";
  s += typeof v === "nonsense" ? "?" : "-";
  s += typeof v !== "Object" ? "." : "-";
  return s;
}

function names(v: any): string {
  const t = typeof v;
  return t + ":" + (t === typeof v) + ":" + t.length;
}

function undeclared(): string {
  // @ts-ignore
  return typeof notDeclaredAnywhere + (typeof notDeclaredAnywhere === "undefined");
}

function main() {
  const vs = kinds();
  for (let i = 0; i < vs.length; i++) console.log(i, compare(vs[i]), names(vs[i]));
  console.log(undeclared());
  let n = 0;
  for (let i = 0; i < 1000; i++) if (typeof vs[i % vs.length] === "function") n++;
  console.log(n);
}
main();
