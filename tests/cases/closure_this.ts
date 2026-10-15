// @native Box m am Sub rec pick
// Arrow functions use the `this` of the function they are in (a box the
// arrows capture, as variables are); a closure may call the function it
// is in; a type parameter in a closure's annotations is not a capture.
class Box {
  _v = 1;
  _fs: (() => number)[] = [];
  constructor() {
    this._fs.push(() => this._v + 1);
    const inner = () => () => this._v * 10;
    this._fs.push(inner());
  }
  m(): number[] {
    return [1, 2].map((x) => x + this._v);
  }
  async am(): Promise<number> {
    await null;
    const f = () => this._v;
    this._v = 7;
    return f();
  }
}
class Sub extends Box {
  constructor() {
    super();
    this._fs.push(() => this._v + 100);
  }
}
const o = {
  v: 5,
  get(): number {
    return [0].map(() => this.v)[0];
  },
};
function rec(n: number): Promise<number> {
  return Promise.resolve(n).then((k) => (k > 0 ? rec(k - 1) : 42));
}
interface Picker<T> {
  last: T | undefined;
  pick: (v: T) => T;
}
function pick<T>(): Picker<T> {
  const p: Picker<T> = {
    last: undefined,
    pick: (v: T): T => {
      p.last = v;
      return v;
    },
  };
  return p;
}
const b = new Box();
console.log(b._fs.map((f) => f()).join(), b.m().join(), o.get());
b.am().then((v) => console.log("am", v));
rec(3).then((v) => console.log("rec", v));
console.log(new Sub()._fs.map((f) => f()).join());
const p = pick<string>();
p.pick("x");
console.log(p.last);
