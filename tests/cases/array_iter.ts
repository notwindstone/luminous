// @native sum grow shrink holes destructure custom subclass breaks
function sum(xs: number[]): number {
  let s = 0;
  for (const x of xs) s += x;
  return s;
}

function grow(xs: number[]): string {
  const seen: number[] = [];
  for (const x of xs) {
    seen.push(x);
    if (xs.length < 6) xs.push(x * 10);
  }
  return seen.join(",");
}

function shrink(xs: number[]): string {
  const seen: number[] = [];
  for (const x of xs) {
    seen.push(x);
    xs.pop();
  }
  return seen.join(",");
}

function holes(xs: unknown[]): string {
  let out = "";
  for (const x of xs) out += String(x) + ",";
  const [a, b, c] = xs;
  return out + "|" + String(a) + "," + String(b) + "," + String(c);
}

function destructure(xs: number[]): string {
  const [a, , b, ...rest] = xs;
  const [x = 100, y = 200] = [];
  return [a, b, rest.length, x, y].join(",");
}

function custom(xs: number[]): number {
  let n = 0;
  for (const x of xs) n += x;
  return n;
}

class Doubled extends Array<number> {
  *[Symbol.iterator]() {
    for (let i = 0; i < this.length; i++) yield this[i] * 2;
  }
}

function subclass(xs: number[]): number {
  let n = 0;
  for (const x of xs) n += x;
  return n;
}

function breaks(xs: number[]): string {
  const out: number[] = [];
  for (const x of xs) {
    if (x === 2) break;
    out.push(x);
  }
  try {
    for (const x of xs) {
      if (x === 3) throw new Error("t");
    }
  } catch (e) {
    out.push(-1);
  }
  const [first] = xs;
  out.push(first);
  return out.join(",");
}

const big: number[] = [];
for (let i = 0; i < 1000; i++) big.push(i);
console.log(sum(big), sum([]), sum([1.5, 2.5]));
console.log(grow([1, 2]), shrink([1, 2, 3, 4, 5, 6]));
Object.defineProperty(Array.prototype, 1, { get() { return "proto"; }, configurable: true });
console.log(holes([0, , 2]), holes([]));
delete (Array.prototype as any)[1];
console.log(destructure([1, 2, 3, 4, 5]));
const own = [1, 2, 3];
(own as any)[Symbol.iterator] = function* () { yield 40; yield 2; };
console.log(custom(own), custom([1, 2]));
console.log(subclass(Doubled.from([1, 2, 3]) as unknown as number[]));
// Replacing array iteration is noticed when a loop starts.
const values = Array.prototype[Symbol.iterator];
(Array.prototype as any)[Symbol.iterator] = function* () { yield 7; };
console.log(sum([1, 2, 3]));
Array.prototype[Symbol.iterator] = values;
const next = Object.getPrototypeOf([][Symbol.iterator]()).next;
Object.getPrototypeOf([][Symbol.iterator]()).next = function () { return { done: true, value: 0 }; };
console.log(sum([1, 2, 3]));
Object.getPrototypeOf([][Symbol.iterator]()).next = next;
console.log(sum([1, 2, 3]));
// A `return` on the iterator prototype chain is called when a loop over an
// array exits early, with the array iterator as `this`.
const iterProto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
const calls: string[] = [];
iterProto.return = function () {
  calls.push(Object.prototype.toString.call(this) + ":" + JSON.stringify(this.next()));
  return {};
};
console.log(breaks([1, 2, 3, 4]), calls.join(" "));
delete iterProto.return;
console.log(breaks([1, 2, 3, 4]));
