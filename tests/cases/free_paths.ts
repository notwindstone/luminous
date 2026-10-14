// @native chain sumChain accessors converted wide holders cycle
// Objects freed when their last reference goes: chains of plain objects
// (freed in a loop), objects with accessors (whose functions are released
// with them), a data property turned into an accessor, objects with a
// separate property array, objects in arrays, and cycles (the collector).

type Link = { value: number; label: string; next: Link | null };

function chain(n: number): Link | null {
  let head: Link | null = null;
  for (let i = 0; i < n; i++) head = { value: i, label: "n" + i, next: head };
  return head;
}

function sumChain(l: Link | null): number {
  let s = 0;
  while (l !== null) {
    s += l.value + l.label.length;
    l = l.next;
  }
  return s;
}

let gets = 0;
function accessors(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const inner = { k: i };
    const o = {
      base: inner,
      get double(): number {
        gets++;
        return inner.k * 2;
      },
      set double(v: number) {
        inner.k = v / 2;
      },
    };
    o.double = o.double + 2;
    total += o.base.k;
  }
  return total;
}

function converted(n: number): string {
  const out: string[] = [];
  for (let i = 0; i < n; i++) {
    const o: { a: number; b: { c: string } } = { a: i, b: { c: "x" + i } };
    const saved = o.b;
    Object.defineProperty(o, "a", { get: () => saved.c.length + i, configurable: true });
    out.push(String(o.a));
  }
  return out.join(",");
}

function wide(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const o: Record<string, { v: number }> = {};
    for (let k = 0; k < 20; k++) o["p" + k] = { v: k + i };
    s += o.p19.v + Object.keys(o).length;
  }
  return s;
}

function holders(n: number): number {
  const xs: { item: { id: number; tags: string[] } }[] = [];
  for (let i = 0; i < n; i++) xs.push({ item: { id: i, tags: ["t" + i, "u"] } });
  let s = 0;
  for (const x of xs) s += x.item.id + x.item.tags.length;
  xs.length = n / 2;
  return s + xs.length;
}

type Node = { name: string; peer: Node | null; data: { n: number } };

function cycle(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const a: Node = { name: "a", peer: null, data: { n: i } };
    const b: Node = { name: "b", peer: a, data: { n: 1 } };
    a.peer = b;
    s += a.peer.data.n + b.peer!.data.n;
  }
  return s;
}

console.log(sumChain(chain(1000)), sumChain(chain(0)));
let keep = chain(50);
keep = chain(3);
console.log(sumChain(keep));
console.log(accessors(1000), gets);
console.log(converted(5));
console.log(wide(200));
console.log(holders(1000));
console.log(cycle(20000));
