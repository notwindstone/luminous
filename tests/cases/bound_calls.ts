// @native self add calls rems ones main
// Bound functions called from native code: one without bound arguments of a
// natively compiled function is that function called on the bound this;
// the others (bound arguments, interpreted or builtin targets) as before.
// And remainders of int32s.
function self(this: any, a?: any, b?: any): string {
  return String(this?.tag) + ":" + a + ":" + b + ":" + arguments.length;
}
function add(a: number, b: number): number {
  return a + b;
}
const interp = new Function("a", "return 'interp:' + this.tag + ':' + a");
function calls(): string[] {
  const o = { tag: "o" };
  const fs: any[] = [
    self.bind(o),
    self.bind(o, 1),
    self.bind(null),
    add.bind(null),
    interp.bind(o),
    Math.max.bind(null),
    self.bind(o).bind({ tag: "other" }),
    "abc".slice.bind("hello"),
  ];
  const out: string[] = [];
  for (const f of fs) out.push(f.name + "/" + f.length + " " + f(7, 8));
  const B: any = function (this: any, x: number) {
    this.x = x;
  }.bind({ ignored: true });
  out.push(String(new B(5).x));
  return out;
}
function rems(xs: any[]): string {
  const out: string[] = [];
  for (const x of xs) for (const y of [1, 2, 3, -1, -3, 0, 7]) out.push(Object.is(x % y, -0) ? "-0" : String(x % y));
  return out.join(",");
}
function ones(xs: number[]): string {
  const out: string[] = [];
  for (let i = 0; i < xs.length; i++) {
    const r = xs[i] % 1;
    out.push(Object.is(r, -0) ? "-0" : String(r));
  }
  return out.join(",");
}
function main() {
  for (const l of calls()) console.log(l);
  console.log(rems([0, 5, -5, -6, 2147483647, -2147483648, 1.5, "9"]));
  console.log(ones([0, -0, 1, -1, 2.5, -2.5, 1e300, -1e300, 2 ** 53 + 2, Infinity, -Infinity, NaN, 0.1, -0.75]));
}
main();
