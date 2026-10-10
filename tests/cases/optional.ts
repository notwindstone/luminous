// @native probe calls effects
type Node = { v?: number; next?: Node | null; f?: (x: number) => number; arr?: number[] };

function probe(n: Node | null | undefined): string {
  const out: unknown[] = [];
  out.push(n?.v, n?.next?.v, n?.next?.next?.v, n?.arr?.[1], n?.arr?.length, n?.["v"]);
  out.push(n?.next!.v);
  return out.map(String).join(",");
}

function calls(o: { m?: (x: number) => number; k: number } | null, f?: (x: number) => number): string {
  const out: unknown[] = [];
  out.push(o?.m?.(1), f?.(2));
  try {
    out.push(o?.m!(3));
  } catch (e) {
    out.push((e as Error).name);
  }
  return out.map(String).join(",");
}

let evaluated = 0;
function effect(): number {
  evaluated++;
  return 1;
}

function effects(n: Node | null): string {
  evaluated = 0;
  const a = n?.f?.(effect());
  const b = n?.arr?.[effect()];
  let thrown = "";
  try {
    // Parentheses end the chain.
    (n?.next).v;
  } catch (e) {
    thrown = (e as Error).name;
  }
  return `${a},${b},${evaluated},${thrown}`;
}

const list: Node = { v: 1, next: { v: 2, next: null }, arr: [10, 20], f: (x) => x * 10 };
console.log(probe(list), "|", probe(null), "|", probe(undefined), "|", probe({ next: { v: 3 } }));
const o = { k: 5, m(this: { k: number }, x: number) { return this.k + x; } };
console.log(calls(o, (x) => x * 2), "|", calls(null), "|", calls({ k: 1 }));
console.log(effects(list), "|", effects(null), "|", effects({ v: 0 }));
