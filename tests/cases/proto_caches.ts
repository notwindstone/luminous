// @native callAll readAll writeAll m m2 viaSuper getSuper bump read
class Base {
  x = 1;
  get v(): number {
    return this.x * 10;
  }
  set v(n: number) {
    this.x = n;
  }
  m(): string {
    return "base.m";
  }
  get g(): string {
    return "base.g of " + this.x;
  }
}
class Mid extends Base {
  m2(): string {
    return "mid.m2";
  }
}
class Leaf extends Mid {
  viaSuper(): string {
    return super.m() + "|" + super.m2();
  }
  getSuper(): string {
    return super.g;
  }
}
class Other {
  m(): string {
    return "other.m";
  }
  get v(): number {
    return -1;
  }
  set v(n: number) {}
}

function callAll(objs: { m(): string }[]): string {
  return objs.map((o) => o.m()).join(",");
}
function readAll(objs: { v: number }[]): string {
  const out: number[] = [];
  for (const o of objs) out.push(o.v);
  return out.join(",");
}
function writeAll(objs: { v: number }[], n: number): string {
  for (const o of objs) o.v = n;
  return readAll(objs);
}

class Counter {
  #n = 0;
  bump(k: number): number {
    for (let i = 0; i < k; i++) this.#n += i;
    return this.#n;
  }
  static read(c: Counter): number {
    return c.#n;
  }
}

const a = new Leaf();
const b = new Leaf();
const log: string[] = [];
for (let round = 0; round < 3; round++) log.push(callAll([a, b, new Mid(), new Other()]));
log.push(readAll([a, new Base(), new Other()]), writeAll([a, b], 7));
// A method replaced on a prototype, then shadowed by an own property.
Mid.prototype.m = function () {
  return "mid.m";
};
log.push(callAll([a, b]));
(b as unknown as { m: () => string }).m = () => "own.m";
log.push(callAll([a, b]));
// An intermediate prototype gains a shadowing method; a getter becomes data.
Object.defineProperty(Mid.prototype, "v", { value: 99, configurable: true, writable: true });
log.push(readAll([a, new Base()]));
delete (Mid.prototype as unknown as Record<string, unknown>).v;
log.push(readAll([a]));
Object.defineProperty(Base.prototype, "v", { get: () => 5, configurable: true });
log.push(readAll([a, new Base()]), writeAll([new Base()], 3));
// A changed prototype chain.
Object.setPrototypeOf(b, Other.prototype);
log.push(callAll([a, b]), readAll([b]));
Object.setPrototypeOf(Leaf.prototype, Base.prototype);
try {
  log.push(a.viaSuper());
} catch (e) {
  log.push((e as Error).constructor.name);
}
Object.setPrototypeOf(Leaf.prototype, Mid.prototype);
log.push(a.viaSuper(), a.getSuper());
Base.prototype.m = function () {
  return "base.m2";
};
Object.defineProperty(Base.prototype, "g", { get: () => "redefined", configurable: true });
log.push(a.viaSuper(), a.getSuper());
console.log(log.join("\n"));

const c1 = new Counter();
const c2 = new Counter();
console.log(c1.bump(10), c2.bump(5), c1.bump(1), Counter.read(c2));
try {
  Counter.read({} as Counter);
} catch (e) {
  console.log((e as Error).constructor.name);
}
const d = Object.getOwnPropertyDescriptor(Base.prototype, "g")!;
const desc = Object.getOwnPropertyDescriptor(Other.prototype, "v")!;
console.log(typeof desc.get, typeof desc.set, desc.get!.name, desc.set!.name, desc.enumerable, desc.configurable, typeof d.get);
