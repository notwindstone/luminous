// @native basics accessors order kinds closures counter
// @interpreted arrowThis superInMethod
function basics(): string {
  const o = {
    value: 2,
    add(x: number, y: number = 10): number {
      return this.value + x + y;
    },
    sum(...xs: number[]): number {
      let s = this.value;
      for (const x of xs) s += x;
      return s;
    },
    self() {
      return this;
    },
  };
  const out: unknown[] = [o.add(1), o.add(1, 2), o.sum(1, 2, 3), o.self() === o];
  out.push(o.add.name, o.add.length, o.sum.length, "prototype" in o.add, typeof o.add);
  try {
    new (o.add as unknown as new () => object)();
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  const extracted = o.add;
  try {
    extracted(1);
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  return out.join(",");
}

function accessors(): string {
  let log = "";
  const o = {
    x: 1,
    get double(): number {
      log += "g";
      return this.x * 2;
    },
    set double(v: number) {
      log += "s";
      this.x = v / 2;
    },
    get only(): string {
      return "only";
    },
    set writeOnly(v: number) {
      this.x = v;
    },
    get boom(): number {
      throw new Error("getter threw");
    },
  };
  o.double = 10;
  const d = o.double;
  o.writeOnly = 7;
  const desc = Object.getOwnPropertyDescriptor(o, "double")!;
  const out: unknown[] = [d, o.x, log, o.only, (o as Record<string, unknown>).writeOnly];
  out.push(desc.get!.name, desc.set!.name, desc.enumerable, desc.configurable, "value" in desc);
  try {
    (o as { only: string }).only = "x";
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  try {
    out.push(o.boom);
  } catch (e) {
    out.push((e as Error).message);
  }
  return out.join(",");
}

function order(): string {
  const o = {
    a: 1,
    get b(): number {
      return 2;
    },
    c() {
      return 3;
    },
    a2: 4,
  };
  const p = {
    k: 1,
    get k2(): number {
      return 5;
    },
  };
  const json = JSON.stringify(o) + JSON.stringify(p);
  const q = {
    v: 1,
    get v2(): number {
      return 1;
    },
    set v2(x: number) {},
  };
  return Object.keys(o).join("") + "|" + json + "|" + Object.keys(q).join("");
}

function kinds(): Promise<string> {
  const o = {
    n: 3,
    *range() {
      for (let i = 0; i < this.n; i++) yield i;
    },
    async later(): Promise<number> {
      await null;
      return this.n * 10;
    },
    async *stream() {
      for (let i = 0; i < this.n; i++) {
        await null;
        yield i * 100;
      }
    },
  };
  const out: unknown[] = [[...o.range()].join("")];
  return (async () => {
    out.push(await o.later());
    const got: number[] = [];
    for await (const x of o.stream()) got.push(x);
    out.push(got.join("+"));
    return out.join(",");
  })();
}

function closures(n: number): number {
  const counter = {
    count: 0,
    makeAdder(k: number) {
      return (x: number) => x + k + n;
    },
    bump() {
      this.count++;
      return this.count;
    },
  };
  const add = counter.makeAdder(5);
  let s = 0;
  for (let i = 0; i < 10; i++) s += add(i) + counter.bump();
  return s;
}

function counter(): number {
  const obj = {
    value: 0,
    get v(): number {
      return this.value;
    },
    set v(x: number) {
      this.value = x;
    },
    add(x: number): number {
      this.value += x;
      return this.value;
    },
  };
  let acc = 0;
  for (let i = 0; i < 1000; i++) {
    obj.v = i;
    acc += obj.add(obj.v);
  }
  return acc;
}

function arrowThis(): number {
  const o = {
    x: 4,
    m() {
      return [1, 2].map(() => this.x).join("");
    },
  };
  return Number(o.m());
}

function superInMethod(): string {
  const base = { hi: () => "base hi" };
  const o = {
    hi() {
      return "own " + super.hi();
    },
  };
  Object.setPrototypeOf(o, base);
  return o.hi();
}

console.log(basics());
console.log(accessors());
console.log(order());
console.log(closures(1), counter(), arrowThis(), superInMethod());
kinds().then((s) => console.log(s));
