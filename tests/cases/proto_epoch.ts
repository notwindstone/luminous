// @native readAll callM getG getX readOwn writeX run churn callM2 getB
// Caches of prototypes' methods and getters keep what the prototype held
// while no watched prototype changes: each kind of change is seen.

class Base {
  m(): string {
    return "base.m";
  }
  get g(): string {
    return "base.g";
  }
}
class Mid extends Base {}
class Leaf extends Mid {
  n(): string {
    return "leaf.n";
  }
}

function callM(o: any): string {
  return o.m();
}
function getG(o: any): string {
  return o.g;
}
function getX(o: any): any {
  return o.x;
}
function readOwn(o: any): any {
  return o.y;
}
function writeX(o: any, v: any): void {
  o.x = v;
}
function readAll(o: any): string {
  return [callM(o), getG(o), String(getX(o))].join(" ");
}

function run(): void {
  const leaf = new Leaf();
  const mid = new Mid();
  for (let i = 0; i < 3; i++) console.log("warm", readAll(leaf), readAll(mid));

  // A method replaced on the holder.
  (Base.prototype as any).m = function () {
    return "base.m2";
  };
  console.log("replaced", readAll(leaf), readAll(mid));

  // Shadowed on an intermediate prototype (a property added).
  (Mid.prototype as any).m = function () {
    return "mid.m";
  };
  console.log("shadowed", readAll(leaf), readAll(mid));

  // Deleted again.
  delete (Mid.prototype as any).m;
  console.log("deleted", readAll(leaf), readAll(mid));

  // A getter redefined (no shape change).
  Object.defineProperty(Base.prototype, "g", { get: () => "base.g2", configurable: true });
  console.log("getter", readAll(leaf), readAll(mid));

  // A data property of a prototype, written through the engine.
  (Base.prototype as any).x = 1;
  console.log("x", readAll(leaf));
  (Base.prototype as any).x = 2;
  console.log("x2", readAll(leaf));

  // Written through a write cache filled before the object was a holder.
  const p: any = { x: 10 };
  for (let i = 0; i < 3; i++) writeX(p, i);
  const child = Object.create(p);
  for (let i = 0; i < 3; i++) console.log("child", getX(child));
  writeX(p, 42);
  console.log("child after write", getX(child));
  writeX(p, 43);
  console.log("child after write", getX(child));

  // An object of the same layout as a watched one (made by the same
  // additions) does not take its shape: a write cache of it does not
  // write the watched one.
  const w: any = {};
  w.x = 1;
  const wc = Object.create(w);
  for (let i = 0; i < 3; i++) console.log("watched", getX(wc));
  const twin: any = {};
  twin.x = 2;
  for (let i = 0; i < 3; i++) writeX(twin, i);
  writeX(w, 99);
  console.log("twin", getX(wc), twin.x);
  writeX(w, 100);
  console.log("twin", getX(wc), twin.x);

  // A prototype's prototype set.
  const other = {
    m() {
      return "other.m";
    },
    get g() {
      return "other.g";
    },
    x: "ox",
  };
  Object.setPrototypeOf(Mid.prototype, other);
  console.log("setproto", readAll(leaf), readAll(mid));
  Object.setPrototypeOf(Mid.prototype, Base.prototype);
  console.log("restored", readAll(leaf), readAll(mid));

  // An own property on the receiver shadows (its shape changes).
  const own: any = new Leaf();
  console.log("own before", callM(own), readOwn(own));
  own.m = () => "own.m";
  own.y = "y";
  console.log("own after", callM(own), readOwn(own));

  // Frozen and sealed prototypes still read.
  Object.freeze(Base.prototype);
  console.log("frozen", readAll(leaf));

  // Many instances of several classes through one site.
  const objs: any[] = [new Leaf(), new Mid(), new Base(), other, Object.create(other)];
  let out = "";
  for (let k = 0; k < 20; k++) out += callM(objs[k % objs.length])[0];
  console.log("poly", out);
}

// A watched prototype changed over and over while others are read: each
// site's entry made again, until it stops keeping one; still right after.
class A {
  a(): number {
    return 1;
  }
  get b(): number {
    return 2;
  }
}
function churn(): void {
  const unrelated: any = { v: 0 };
  const sub = Object.create(unrelated);
  const x = new A();
  let total = 0;
  for (let i = 0; i < 200; i++) {
    total += getX(sub) === i ? 0 : 1000;
    total += callM2(x) + getB(x);
    unrelated.v = i + 1;
    unrelated.x = i + 1;
    if (i === 150) {
      (A.prototype as any).a = () => 10;
    }
  }
  console.log("churn", total, callM2(x), getB(x), getX(sub));
}
function callM2(o: any): number {
  return o.a();
}
function getB(o: any): number {
  return o.b;
}
run();
churn();
