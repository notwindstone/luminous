// @native make step total poly frozen withGetter extra moved classes
// Fast field access for objects of annotated record types (named object
// types, interfaces, classes made natively): objects with the record's
// shape are read and written at fixed slots; any other object (other
// order, missing or extra fields, accessors, frozen, not an object) goes
// through the inline caches with the same results.

type Body = { x: number; y: number; vx: number; vy: number; mass: number };
interface Named {
  name: string;
  tags: string[];
}

class Particle {
  pos: Body;
  label: string;
  constructor(pos: Body, label: string) {
    this.pos = pos;
    this.label = label;
  }
}

function make(n: number): Body[] {
  const out: Body[] = [];
  for (let i = 0; i < n; i++) out.push({ x: i, y: i * 2, vx: 1, vy: -1, mass: 1 + (i % 3) });
  return out;
}

function step(bs: Body[], dt: number): void {
  for (let i = 0; i < bs.length; i++) {
    const b = bs[i];
    b.x += b.vx * dt;
    b.y += b.vy * dt;
    b.vx -= b.x * 0.01 * b.mass;
  }
}

function total(bs: Body[]): number {
  let s = 0;
  for (const b of bs) s += b.x * b.mass + b.y;
  return s;
}

function poly(): string {
  // Same fields, another order; extra fields; missing fields.
  const a = { y: 1, x: 2, vx: 3, vy: 4, mass: 5 } as Body;
  const b = { x: 1, y: 2, vx: 3, vy: 4, mass: 5, extra: 6 } as Body;
  const c = { x: 7 } as unknown as Body;
  const bs = [a, b, c];
  step(bs, 1);
  return JSON.stringify(bs) + " " + total(bs);
}

function frozen(): string {
  const f = Object.freeze({ x: 1, y: 2, vx: 3, vy: 4, mass: 5 }) as Body;
  const g: Body = { x: 1, y: 2, vx: 3, vy: 4, mass: 5 };
  Object.defineProperty(g, "mass", { value: 9, writable: false });
  let err = "";
  try {
    "use strict";
    step([f], 1);
  } catch (e) {
    err = (e as Error).constructor.name;
  }
  step([g], 1);
  return [JSON.stringify(f), err, JSON.stringify(g)].join(" ");
}

function withGetter(): string {
  let reads = 0;
  const o = {
    get x() {
      reads++;
      return 10;
    },
    y: 0,
    vx: 1,
    vy: 1,
    mass: 1,
  } as unknown as Body;
  return [total([o]), reads].join(" ");
}

function extra(n: Named): string {
  n.tags.push(n.name);
  return n.name + ":" + n.tags.join("|");
}

function moved(p: Particle, dt: number): string {
  p.pos.x += dt;
  return p.label + "@" + p.pos.x.toFixed(2) + "," + p.pos.y;
}

function classes(): string {
  const ps = make(3).map((b, i) => new Particle(b, "p" + i));
  const out = ps.map((p) => moved(p, 0.5));
  // Not an object, and null.
  let err = "";
  try {
    moved(null as unknown as Particle, 1);
  } catch (e) {
    err = (e as Error).constructor.name;
  }
  return out.join(" ") + " " + err;
}

const bs = make(100);
for (let k = 0; k < 10; k++) step(bs, 0.1);
console.log(total(bs).toFixed(6));
console.log(poly());
console.log(frozen());
console.log(withGetter());
console.log(extra({ name: "n", tags: ["a"] }), extra({ tags: ["b"], name: "m" } as Named));
console.log(classes());
