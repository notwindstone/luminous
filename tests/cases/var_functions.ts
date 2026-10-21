// @native access readSync later recur main
// Functions bound by `var` or `let` (as bundlers and Bun's modules write
// them: `var a = function a() {...}, b = ...`) compile natively when the
// binding is never assigned again; one assigned again, or declared again,
// stays interpreted.
const before = typeof recur;
var access = function access(path: any, mode?: any): string {
    return typeof mode === "string" ? "s:" + path : "o:" + path;
  },
  readSync = function readSync(fd: any, buffer: any, offsetOrOptions?: any): string {
    if (arguments.length <= 2 || typeof offsetOrOptions === "object") return "opts " + arguments.length;
    return "pos " + arguments.length;
  },
  plain = 5,
  bound = access.bind(null, "bound");
let later = function (x: number): number {
  return x * 2;
};
var recur = function (n: number): number {
  return n <= 1 ? 1 : n * recur(n - 1);
};
let counter = (() => {
  let n = 0;
  return () => ++n;
})();
var reassigned = function (): string {
  return "first";
};
reassigned = function (): string {
  return "second";
};
var twice = function (): number {
  return 1;
};
var twice = function (): number {
  return 2;
};
function main() {
  const o = { access, readSync, bound, later, recur, counter, reassigned, twice };
  console.log(before, typeof later, later.name, access.name, readSync.name, recur.name, reassigned.name);
  console.log(o.access(1), o.access(2, "x"), o.readSync(1, 2), o.readSync(1, 2, 3), o.readSync(1, 2, {}), o.bound("m"), plain);
  console.log(o.later(21), o.recur(5), counter(), counter(), o.counter(), o.reassigned(), o.twice());
}
main();
