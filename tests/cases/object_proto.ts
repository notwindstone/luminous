// @native literals main
// `__proto__: value` in an object literal sets the prototype (an object or
// null; another value is ignored); a shorthand `__proto__` or a method of
// that name is an own property (a shorthand `{ __proto__ }` too, by the
// specification; Bun's transpiler makes it `__proto__: __proto__`).
const base = { greet(): string { return "hi " + (this as any).name; } };
function literals(): string[] {
  const out: string[] = [];
  const a: any = { __proto__: null, x: 1 };
  out.push(String(Object.getPrototypeOf(a)) + " " + Object.keys(a).join(",") + " " + ("toString" in a));
  const b: any = { name: "b", __proto__: base, y: 2 };
  out.push(b.greet() + " " + (Object.getPrototypeOf(b) === base) + " " + Object.keys(b).join(","));
  const c: any = { __proto__: 5, z: 3 };
  out.push(String(Object.getPrototypeOf(c) === Object.prototype) + " " + Object.keys(c).join(","));
  const d: any = { "__proto__": base, name: "d" };
  out.push(d.greet());
  const f: any = { __proto__() { return "method"; } };
  out.push(typeof f.__proto__ + " " + Object.keys(f).join(","));
  const order: string[] = [];
  const g: any = { a: order.push("a"), __proto__: (order.push("proto"), null), b: order.push("b") };
  out.push(order.join(",") + " " + Object.keys(g).join(","));
  let n = 0;
  for (let i = 0; i < 1000; i++) {
    const r: any = { __proto__: null, bytesWritten: i, buffer: null };
    n += r.bytesWritten;
  }
  out.push(String(n));
  return out;
}
function main() {
  for (const l of literals()) console.log(l);
}
main();
