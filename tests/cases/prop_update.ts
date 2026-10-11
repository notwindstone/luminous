// @native bump add sub sweep overflow floats strings missing accessor frozen inherited zeros asValue
function bump(o: { n: number }): number {
  o.n++;
  return o.n;
}

function add(o: { n: number }, k: number): number {
  o.n += k;
  o.n += 2;
  return o.n;
}

function sub(o: { n: number }, k: number): number {
  o.n -= k;
  o.n--;
  return o.n;
}

function sweep(o: { n: number }): number {
  for (let i = 0; i < 100000; i++) o.n++;
  return o.n;
}

function overflow(): string {
  const o = { n: 2147483646 };
  const p = o as { n: number };
  bump(p);
  bump(p);
  bump(p);
  const q = { n: -2147483647 };
  sub(q, 1);
  return p.n + "," + q.n + "," + Number.isInteger(p.n);
}

function floats(): string {
  const o = { n: 0.5 };
  bump(o);
  add(o, 0.25);
  sub(o, 2.75);
  return String(o.n);
}

function strings(): string {
  const o = { n: "a" } as unknown as { n: number };
  add(o, 1);
  const p = { n: "5" } as unknown as { n: number };
  bump(p);
  return o.n + "," + p.n + "," + typeof p.n;
}

function missing(): string {
  const o = {} as { n: number };
  bump(o);
  const p = {} as { n: number };
  add(p, 1);
  return o.n + "," + p.n + "," + Object.keys(o).join("");
}

function accessor(): string {
  let stored = 1;
  const o = {} as { n: number };
  Object.defineProperty(o, "n", {
    get: () => stored * 10,
    set: (v: number) => {
      stored = v;
    },
  });
  bump(o);
  add(o, 1);
  return stored + "," + o.n;
}

function frozen(): string {
  const o = Object.freeze({ n: 1 }) as { n: number };
  try {
    bump(o);
    return "no throw";
  } catch (e) {
    return (e as Error).constructor.name + "," + o.n;
  }
}

function inherited(): string {
  const proto = { n: 40 };
  const o = Object.create(proto) as { n: number };
  bump(o);
  return o.n + "," + proto.n + "," + Object.keys(o).join("");
}

function zeros(): string {
  const a = { n: 0 };
  sub(a, -1);
  const b = { n: -0 };
  add(b, -2);
  const c = { n: -0 } as { n: number };
  c.n -= 0;
  const d = { n: 0 } as { n: number };
  d.n += -0;
  return [a.n, b.n, Object.is(c.n, -0), Object.is(d.n, -0)].join(",");
}

class Counter {
  count = 0;
  tick(): number {
    this.count++;
    this.count += 0.5;
    return this.count;
  }
}

function asValue(o: { n: number }): string {
  const r = (o.n += 3);
  const s = (o.n -= 1.5);
  return r + "," + s + "," + o.n;
}

console.log(bump({ n: 1 }), add({ n: 1 }, 2), sub({ n: 1 }, 5), sweep({ n: 0 }));
// The same sites with differently shaped objects.
console.log(bump({ m: 0, n: 7 } as { n: number }), add({ x: 1, n: 1 } as { n: number }, 3));
console.log(overflow());
console.log(floats());
console.log(strings());
console.log(missing());
console.log(accessor());
console.log(frozen());
console.log(inherited());
console.log(zeros());
const c = new Counter();
c.tick();
console.log(c.tick(), c.count);
console.log(asValue({ n: 1 }), asValue({ n: 0.25 }), asValue({ n: "x" } as unknown as { n: number }));
