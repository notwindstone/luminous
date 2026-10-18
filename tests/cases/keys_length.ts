// @native count countAll errorOf
// `Object.keys(x).length`: counted without making the keys while
// `Object.keys` is the builtin (an object of an `aot type`: its fields
// present; an ordinary object or a dense array: its shape), else as JS.
type int = number;
aot type A = { a: int; b?: string; c: number | null };

function count(x: unknown): number {
  return Object.keys(x as object).length;
}

function errorOf(f: () => unknown): string {
  try {
    return String(f());
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

class P {
  #secret = 1;
  x = 1;
  y = 2;
  get z(): number {
    return this.#secret;
  }
}

function countAll(): string {
  const deleted: any = { a: 1, b: 2, c: 3 };
  delete deleted.b;
  const sym: any = { a: 1, [Symbol("s")]: 2 };
  const hidden: any = { a: 1 };
  Object.defineProperty(hidden, "b", { value: 2, enumerable: false });
  Object.defineProperty(hidden, "c", { get: () => 3, enumerable: true });
  const holes = [1, , 3];
  const sparse: any[] = [];
  sparse[5] = 1;
  const extra: any = [1, 2, 3];
  extra.name = "x";
  const nonEnumArr = [1, 2];
  Object.defineProperty(nonEnumArr, "q", { value: 1, enumerable: false });
  const proxy = new Proxy({ a: 1 }, { ownKeys: () => ["a", "b", "c"], getOwnPropertyDescriptor: () => ({ enumerable: true, configurable: true, value: 1 }) });
  const a1: A = { a: 1, c: null };
  const a2: A = { a: 1, b: "x", c: 2 };
  a2.b = undefined;
  const a3: A = { a: 1, b: "y", c: 3 };
  const as: A[] = [a1, a2, a3];
  function args(..._: unknown[]): number {
    return count(arguments);
  }
  const values: unknown[] = [
    {}, { a: 1, b: 2 }, deleted, sym, hidden, holes, sparse, extra, nonEnumArr, new Array(4), [], "abc", "", 42, true,
    new String("ab"), new Uint8Array(3), new Map([[1, 2]]), () => 1, proxy, new P(), a1, a2, a3, as, Object.create({ inherited: 1 }),
    Object.freeze({ f: 1, g: 2 }), JSON.parse('{"x":1,"y":[1,2],"z":{}}'),
  ];
  return values.map((v) => count(v)).join(",") + " " + args(1, 2, 3);
}

console.log(countAll());
console.log(errorOf(() => count(null)), errorOf(() => count(undefined)));
const saved = Object.keys;
(Object as any).keys = (o: object) => ["only", "this"].concat(saved(o));
console.log(count({ a: 1 }), count([]));
(Object as any).keys = () => ({ length: "not a number" });
console.log(count({ a: 1 }));
(Object as any).keys = saved;
console.log(count({ a: 1, b: 2 }));
