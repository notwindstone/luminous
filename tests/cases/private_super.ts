// @native label describe make bump tryBrand swap total walk setLabel keyed sumAll
class Base {
  label(): string {
    return "base";
  }
  set tag(v: string) {
    (this as any)._tag = "base:" + v;
  }
  get tag(): string {
    return (this as any)._tag;
  }
  static make(): string {
    return "Base.make";
  }
}

class Box extends Base {
  #value: number;
  #items: number[] = [];
  static #count = 0;
  #log(s: string): string {
    return `[${s}]`;
  }
  static #fresh(): number {
    return ++Box.#count;
  }
  get #twice(): number {
    return this.#value * 2;
  }
  set #twice(v: number) {
    this.#value = v / 2;
  }
  constructor(v: number) {
    super();
    this.#value = v;
    Box.#fresh();
  }
  label(): string {
    return "box<" + super.label() + ">";
  }
  describe(): string {
    const get = (b: Box) => b.#value;
    return this.#log(`${get(this)} ${this.#twice} ${#value in this} ${#log in this}`);
  }
  static make(): string {
    return "Box+" + super.make() + " #" + Box.#fresh();
  }
  bump(): string {
    this.#value += 10;
    this.#value++;
    ++this.#value;
    this.#twice = 100;
    [this.#value, ...this.#items] = [7, 8, 9];
    ({ v: this.#value } = { v: this.#value + 1 });
    return `${this.#value} ${this.#items}`;
  }
  tryBrand(o: unknown): string {
    const out: string[] = [];
    for (const f of [() => (o as Box).#value, () => (o as Box).#log("x"), () => (o as Box).#twice]) {
      try {
        out.push(String(f()));
      } catch (e) {
        out.push((e as Error).message);
      }
    }
    out.push(String(#value in (o as object)));
    return out.join(" / ");
  }
  swap(other: Box): void {
    [this.#value, other.#value] = [other.#value, this.#value];
  }
  total(...others: Box[]): number {
    let t = this.#value;
    for (const o of others) t += o.#value;
    return t;
  }
  *walk(): Generator<string> {
    yield super.label();
    yield this.#log(String(this.#value));
    for (const i of this.#items) yield String(i);
  }
  setLabel(v: string): string {
    super.tag = v;
    return super.tag + " " + (this as any)._tag;
  }
  keyed(k: string): unknown {
    return super[k as "label"]();
  }
  static sumAll(...boxes: Box[]): number {
    return boxes.reduce((s, b) => s + b.#value, 0) + Box.#count;
  }
}

const a = new Box(1);
const b = new Box(2);
console.log(a.label(), a.describe(), Box.make());
console.log(a.bump(), a.describe());
console.log(a.tryBrand(b), "|", a.tryBrand({}), "|", a.tryBrand(Object.create(Box.prototype)));
a.swap(b);
console.log(a.total(), a.total(b, b), [...a.walk()], [...b.walk()]);
console.log(a.setLabel("x"), a.keyed("label"), Box.sumAll(a, b));
try {
  a.tryBrand(null);
} catch (e) {
  console.log("null:", (e as Error).name, (e as Error).message);
}
