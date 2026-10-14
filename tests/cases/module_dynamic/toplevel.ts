let total = 0;
for (var i = 0; i < 4; i++) total += i;
var keys: string[] = [];
for (var k in { x: 1, y: 2 }) keys.push(k);
for (const v of [10, 20]) total += v;
if (total > 0) {
  var big = true;
} else {
  var big = false;
}
const { p, q: [r, s] } = { p: 1, q: [2, 3] };
let [m, n = 5] = [4];
label: for (;;) {
  break label;
}
switch (total) {
  case 36:
    var hit = "yes";
    break;
  default:
    hit = "no";
}
try {
  throw new Error("x");
} catch {
  total += 1;
}
const early = later();
function later(): number {
  return 42;
}
class Counter {
  static made = 0;
  n: number;
  constructor() {
    this.n = ++Counter.made;
  }
}
class Noisy {
  static tag = (console.log("Noisy evaluated"), "noisy");
}
export const c1 = new Counter(),
  c2 = new Counter();
let unset: string | undefined;
const value = "toplevel's own value";
export function summary(): string {
  return [total, i, keys.join(""), big, p, r, s, m, n, hit, early, c2.n, Noisy.tag, typeof unset, value].join(",");
}
