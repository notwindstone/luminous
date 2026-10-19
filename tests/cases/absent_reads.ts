// @native readZ countMissing
// Reads of a property objects do not have, nor their prototypes, cached
// with the prototype epoch (the site's first entry, checked inline):
// `undefined` while no watched prototype changes, then what a prototype
// has since (a data property, a getter), for literals, class instances
// and objects without a prototype.
function readZ(o: any): any {
  return o.z;
}

function countMissing(o: any, n: number): number {
  let c = 0;
  for (let i = 0; i < n; i++) if (o.zz === undefined) c++;
  return c;
}

class Point {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
}

const lit: any = { a: 1 };
const bare: any = Object.create(null);
bare.a = 1;
const pt: any = new Point(1, 2);

console.log(readZ(lit), readZ(lit), countMissing(lit, 10));
console.log(readZ(bare), readZ(bare), countMissing(bare, 10));
console.log(readZ(pt), readZ(pt));
// A data property of Object.prototype: every object with it on its chain.
(Object.prototype as any).z = "proto z";
console.log(readZ(lit), readZ(pt), readZ(bare));
(Object.prototype as any).z = "proto z2";
console.log(readZ(lit), readZ(pt));
delete (Object.prototype as any).z;
console.log(readZ(lit), readZ(pt));
// A getter of the class's prototype.
Object.defineProperty(Point.prototype, "z", {
  get() {
    return "getter " + this.x;
  },
  configurable: true,
});
console.log(readZ(pt), readZ(lit));
delete (Point.prototype as any).z;
console.log(readZ(pt), readZ(lit));
// An own property: another shape; the first one is still missing it.
lit.z = "own";
console.log(readZ(lit), readZ({ a: 2 }), readZ(pt));
// Many shapes at one site.
const shapes: any[] = [{}, { q: 1 }, { r: 1 }, { s: 1 }, { t: 1 }, { u: 1 }, { z: 9 }, bare, pt];
let out = "";
for (let k = 0; k < 3; k++) for (const s of shapes) out += String(readZ(s)) + " ";
console.log(out);
// A prototype that keeps changing (past the times an entry is made again).
let flips = "";
for (let i = 0; i < 200; i++) {
  if (i % 2) (Object.prototype as any).zz = i;
  else delete (Object.prototype as any).zz;
  flips += countMissing({ a: 1 }, 2);
}
delete (Object.prototype as any).zz;
console.log(flips.length, flips.slice(0, 20), flips.slice(-20), countMissing({ a: 1 }, 3));
// A new prototype.
const child: any = { a: 1 };
console.log(readZ(child));
Object.setPrototypeOf(child, { z: "from the new prototype" });
console.log(readZ(child), readZ({ a: 3 }));
// A proxy on the chain.
const viaProxy: any = Object.create(new Proxy({}, { get: (_t, k) => "proxy " + String(k) }));
console.log(readZ(viaProxy), readZ(viaProxy), readZ(lit));
// Object.prototype's own `z` made, then a null-prototype object's.
bare.z = "bare z";
console.log(readZ(bare), readZ(Object.create(null)));
