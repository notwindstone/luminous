// @native run poly twice statics literal deep patchLabel
// Calls, getters and setters predicted from `this`'s class, `super` and
// object literals are inlined behind a check: every misprediction must
// still reach the right function.

class Base {
  x = 0;
  get value(): number {
    return this.x;
  }
  set value(v: number) {
    this.x = v;
  }
  step(v: number): number {
    return v + 1;
  }
  label(): string {
    return "base:" + this.x;
  }
  fail(n: number): number {
    if (n > 2) throw new Error("fail " + n);
    return n;
  }
}

class Derived extends Base {
  y = 0;
  step(v: number): number {
    return super.step(v) + this.y;
  }
  run(n: number): string {
    const out: unknown[] = [];
    let acc = 0;
    for (let i = 0; i < n; i++) {
      this.value = i;
      this.y = i * 2;
      acc += this.step(this.value);
      if (i === 3) {
        // Replace a method on the prototype: the check fails from now on.
        patchLabel();
      }
      out.push(this.label());
      try {
        acc += this.fail(i);
      } catch (e) {
        out.push((e as Error).message);
      }
    }
    // An argument of another type: the native entry would decline.
    out.push(this.step("s" as unknown as number));
    return acc + "|" + out.join(",");
  }
  static make(): Derived {
    return new this();
  }
  static twice(n: number): number {
    return this.half(n) * 4;
  }
  static half(n: number): number {
    return n / 2;
  }
}

class Other extends Derived {
  step(v: number): number {
    return v * 100;
  }
  get value(): number {
    return -this.x;
  }
  set value(v: number) {
    this.x = v * 3;
  }
}

function patchLabel(): void {
  Derived.prototype.label = () => "patched";
}

function run(): string {
  return new Derived().run(6);
}

function poly(): string {
  // The same call sites see a subclass that overrides them.
  return new Other().run(4);
}

function twice(): string {
  const d = new Derived();
  Object.setPrototypeOf(Derived.prototype, { step: (v: number) => v - 1, label: () => "swapped", fail: (n: number) => n });
  const r = d.run(2);
  Object.setPrototypeOf(Derived.prototype, Base.prototype);
  return r + "|" + d.run(2);
}

function statics(): string {
  const a = Derived.twice(10);
  const original = Derived.half;
  Derived.half = (n: number) => n;
  const b = Derived.twice(10);
  Derived.half = original;
  return [a, b, Derived.make() instanceof Derived].join(",");
}

function literal(): string {
  const out: unknown[] = [];
  const o = {
    value: 1,
    get v(): number {
      return this.value;
    },
    set v(n: number) {
      this.value = n;
    },
    add(n: number): number {
      this.value += n;
      return this.value;
    },
  };
  const other = { value: 1000, add: o.add };
  for (let i = 0; i < 6; i++) {
    o.v = i;
    out.push(o.add(o.v), other.add(i));
    if (i === 2) o.add = (n: number) => -n;
    if (i === 3) Object.defineProperty(o, "v", { get: () => 42, set: (n: number) => out.push("set " + n), configurable: true });
  }
  return out.join(",");
}

function deep(): string {
  const o = {
    n: 0,
    inc(): number {
      return ++this.n;
    },
    twice(): number {
      return this.inc() + this.inc();
    },
  };
  let s = 0;
  for (let i = 0; i < 5; i++) s += o.twice();
  return s + "," + o.n;
}

console.log(run());
console.log(poly());
console.log(twice());
console.log(statics());
console.log(literal());
console.log(deep());
