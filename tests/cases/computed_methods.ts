// @native make main
// Methods, getters and setters with computed keys in object literals: the
// key evaluated in order, the function named after it.
const sym = Symbol("tag");
const anon = Symbol();
const empty = Symbol("");
function make(prefix: string): any {
  let set = "";
  const order: string[] = [];
  const o: any = {
    __proto__: null,
    a: (order.push("a"), 1),
    [(order.push("key"), prefix + "M")](x: number) {
      return this.a + x;
    },
    get [prefix + "G"]() {
      return "got " + this.a;
    },
    set [prefix + "G"](v: string) {
      set = v;
    },
    [sym]() {
      return "sym";
    },
    [anon]() {
      return "anon";
    },
    [empty]() {
      return "empty";
    },
    async [Symbol.asyncIterator]() {
      return 1;
    },
    *[Symbol.iterator]() {
      yield 1;
      yield 2;
    },
    [42]() {
      return 42;
    },
    b: (order.push("b"), 2),
  };
  o[prefix + "G"] = "assigned";
  return { o, set, order };
}
function main() {
  const { o, set, order } = make("p");
  console.log(order.join(","), set, o.pM(5), o.pG, o[sym](), o[anon](), o[empty](), o[42](), [...o].join());
  const names = [o.pM.name, Object.getOwnPropertyDescriptor(o, "pG")!.get!.name, Object.getOwnPropertyDescriptor(o, "pG")!.set!.name, o[sym].name, JSON.stringify(o[anon].name), o[empty].name, o[Symbol.asyncIterator].name, o[Symbol.iterator].name, o[42].name];
  console.log(names.join("|"));
  console.log(Reflect.ownKeys(o).map(String).join(","), Object.keys(o).join(","), typeof o.pM.prototype);
}
main();
