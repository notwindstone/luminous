// @native V dist normLen chain mutate loop patched swapped identity passed
// @interpreted replaceDot restoreDot replaceSub restoreSub
// New class instances that do not escape keep their fields in locals
// (scalar replacement): field reads and writes, methods inline over them;
// a replaced method or a different producer makes the object first.

class V {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  add(o: V): V {
    return new V(this.x + o.x, this.y + o.y);
  }
  sub(o: V): V {
    return new V(this.x - o.x, this.y - o.y);
  }
  scale(k: number): V {
    return new V(this.x * k, this.y * k);
  }
  dot(o: V): number {
    return this.x * o.x + this.y * o.y;
  }
  len(): number {
    return Math.sqrt(this.dot(this));
  }
  norm(): V {
    return this.scale(1 / this.len());
  }
}

let kept: V[] = [];
const origDot = V.prototype.dot;
const origSub = V.prototype.sub;

function replaceDot(): void {
  "use no aot";
  V.prototype.dot = function (o) {
    kept.push(this);
    return 1000 + origDot.call(this, o);
  };
}

function restoreDot(): void {
  "use no aot";
  V.prototype.dot = origDot;
}

function replaceSub(): void {
  "use no aot";
  V.prototype.sub = function (o) {
    // Not a new instance: this one, changed.
    this.x -= o.x;
    this.y -= o.y;
    return this;
  };
}

function restoreSub(): void {
  "use no aot";
  V.prototype.sub = origSub;
}

function dist(a: V, b: V): number {
  const d = a.sub(b);
  return Math.sqrt(d.dot(d));
}

function normLen(a: V): number {
  const n = a.norm();
  return n.len() + n.x;
}

function chain(a: V, b: V): number {
  const c = a.add(b).scale(2);
  return c.dot(a) + a.sub(b).len();
}

function passed(a: V, b: V): number {
  // Passed to methods of other objects: made only if they are called.
  const d = a.sub(b);
  return b.dot(d) + a.add(d).len() + a.add(b.scale(2)).x;
}

function mutate(a: V): string {
  const m = a.scale(1);
  m.x = 10;
  m.y += 5;
  return [m.x, m.y, m.dot(m), a.x].join(",");
}

function loop(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const p = new V(i, i + 1);
    const q = p.add(p);
    s += q.dot(p) - p.len();
  }
  return s;
}

function patched(a: V, b: V): string {
  kept = [];
  replaceDot();
  // The objects are made for the replacement, which keeps them.
  const r1 = dist(a, b);
  const r2 = normLen(a);
  const r3 = passed(a, b);
  restoreDot();
  const same = kept.length > 0 && kept[0] instanceof V;
  return [r1.toFixed(3), r2.toFixed(3), r3.toFixed(3), kept.length, same, JSON.stringify(kept[0])].join(" ");
}

function swapped(a: V, b: V): string {
  // The producer returns another object: used as it is.
  const a2 = new V(a.x, a.y);
  replaceSub();
  const r = dist(a2, b);
  restoreSub();
  return r.toFixed(3) + " " + JSON.stringify(a2);
}

function identity(a: V): string {
  kept = [];
  replaceDot();
  const m = a.scale(3);
  const d = m.dot(a);
  restoreDot();
  // The object the replacement saw is the one the local holds afterwards.
  m.x = 77;
  return [d, kept[0].x, m.x, m.y, kept[0].y].join(" ");
}

const a = new V(3, 4);
const b = new V(1, 1);
console.log(dist(a, b).toFixed(6), normLen(a).toFixed(6), chain(a, b).toFixed(6));
console.log(mutate(a), loop(100).toFixed(3), passed(a, b).toFixed(6));
console.log(patched(a, b));
console.log(swapped(a, b));
console.log(identity(a));
console.log(dist(a, b).toFixed(6), JSON.stringify(a), JSON.stringify(b));
