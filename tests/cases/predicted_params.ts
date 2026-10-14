// @native scale f1 f2 f3 norm half
// Methods inlined where `this` predicts them (speculative inlining) take
// their arguments as given: a `number` parameter of the inlined body holds
// fractions too.
class V {
  x: number;
  constructor(x: number) {
    this.x = x;
  }
  scale(k: number): V {
    return new V(this.x * k);
  }
  f1(): V {
    return this.scale(0.5);
  }
  f2(k: number): V {
    return this.scale(k);
  }
  f3(): number {
    let s = 0;
    for (let i = 0; i < 4; i++) s += this.scale(i + 0.25).x;
    return s;
  }
  norm(): V {
    return this.scale(1 / Math.sqrt(this.x * this.x));
  }
  half(flag: boolean): number {
    return flag ? this.scale(-0.5).x : this.scale(1e-300).x;
  }
}
const v = new V(3);
console.log(v.f1().x, v.f2(0.5).x, v.f2(-2.75).x, v.f3(), v.norm().x, v.half(true), v.half(false));
