// @native setTo77 viaMethod fromProto viaSetter coerced proxied guarded aliased bumpX viaCallee nested reassigned ownGetter deleted protoSwap shadowed nestedLoops viaForEach
// Property reads a loop does not change are made once before it (effect
// summaries; see src/reuse.rs), and still give what a read in the loop
// gives: after a getter is added to a prototype mid-run by a setter, a
// coercion that writes the property, through a proxy, with a loop that
// does not run reading a property of null, through an alias, from a
// callee, when the variable or the prototype changes, or the property is
// deleted or shadowed.
type Box = { x: number; trigger?: number };

// A prototype's data property; a setter the loop runs makes it a getter.
const proto: any = { x: 1 };
Object.defineProperty(proto, "trigger", {
  set(v: number) {
    Object.defineProperty(proto, "x", { get: () => 100 + v, configurable: true });
  },
  configurable: true,
});
function fromProto(o: Box, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.x;
  return s;
}
function viaSetter(o: Box, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += o.x;
    if (i === 3) o.trigger = i;
  }
  return s;
}

// A coercion of another value (its valueOf) writes the property.
function coerced(o: Box, k: any, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.x * k;
  return s;
}

// A proxy's get trap runs each time.
function proxied(o: Box, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.x;
  return s;
}

// No iteration: reading a property of null does not throw.
function guarded(cfg: Box | null, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += (cfg as Box).x;
  return s;
}

// Writes through another reference, in the loop and in callees.
function aliased(o: Box, n: number): number {
  const a = o;
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += o.x;
    a.x = i;
  }
  return s;
}
function bumpX(q: Box): number {
  q.x++;
  return q.x;
}
function viaCallee(o: Box, other: Box, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.x + bumpX(other);
  return s;
}
function deeper(q: Box): number {
  return bumpX(q) * 2;
}
function nested(o: Box, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.x + deeper(o);
  return s;
}

// A method (a native function, called directly through the value) writes
// it.
function setTo77(q: Box): number {
  q.x = 77;
  return 0;
}
function viaMethod(o: Box, h: { m: (q: Box) => number }, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.x + (i === 1 ? h.m(o) : 0);
  return s;
}

// The module variable itself changes.
let current: Box = { x: 5 };
const spare: Box = { x: 50 };
function swap(): number {
  current = spare;
  return 0;
}
function reassigned(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += current.x + (i === 2 ? swap() : 0);
  return s;
}

// An own getter: read each time.
function ownGetter(o: Box, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.x;
  return s;
}

// Deleted: then the prototype's, or nothing.
function deleted(o: any, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += o.x === undefined ? -1 : o.x;
    if (i === 1) delete o.x;
  }
  return s;
}

// The prototype changes.
function protoSwap(o: any, p2: any, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += o.x;
    if (i === 1) Object.setPrototypeOf(o, p2);
  }
  return s;
}

// An own property shadows the prototype's, added by a setter of another
// object.
function shadowed(o: any, w: any, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += o.x;
    if (i === 2) w.target = o;
  }
  return s;
}

// Loops in loops; a callback that writes.
function nestedLoops(o: Box, n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    for (let j = 0; j < n; j++) s += o.x * j;
    o.x += 1;
  }
  return s;
}
function viaForEach(o: Box, xs: number[]): number {
  let s = 0;
  for (let i = 0; i < 3; i++) {
    s += o.x;
    xs.forEach((v) => {
      if (v > 2) o.x = v;
    });
  }
  return s;
}

const fromProtoObj: Box = Object.create(proto);
console.log("fromProto", fromProto(fromProtoObj, 5));
console.log("viaSetter", viaSetter(fromProtoObj, 8), fromProtoObj.x);

const c: Box = { x: 2 };
const k = {
  valueOf() {
    c.x += 10;
    return 3;
  },
};
console.log("coerced", coerced(c, k, 4), c.x);

const log: string[] = [];
const p = new Proxy({ x: 7 } as Box, {
  get(t: any, key: string) {
    log.push(String(key));
    return t[key] * 2;
  },
});
console.log("proxied", proxied(p, 3), log.join(","));

console.log("guarded", guarded(null, 0));
try {
  guarded(null, 1);
} catch (e) {
  console.log("guarded threw", (e as Error).constructor.name);
}
try {
  guarded(undefined as any, 2);
} catch (e) {
  console.log("guarded threw", (e as Error).constructor.name);
}

console.log("aliased", aliased({ x: 9 }, 4));
const shared: Box = { x: 1 };
console.log("viaCallee same", viaCallee(shared, shared, 4), shared.x);
console.log("viaCallee other", viaCallee({ x: 3 }, { x: 0 }, 4));
console.log("nested", nested({ x: 1 }, 4));
console.log("reassigned", reassigned(5));
console.log("viaMethod", viaMethod({ x: 1 }, { m: setTo77 }, 4));

let reads = 0;
const g = {
  get x() {
    return ++reads;
  },
};
console.log("ownGetter", ownGetter(g as Box, 4), reads);

const base = { x: 1000 };
const d = Object.create(base);
d.x = 1;
console.log("deleted", deleted(d, 4), deleted({ x: 2 }, 4));

const p1 = { x: 1 };
const p2 = { x: 2 };
console.log("protoSwap", protoSwap(Object.create(p1), p2, 4));

const sh = Object.create({ x: 1 });
const w = {
  set target(o: any) {
    o.x = 40;
  },
};
console.log("shadowed", shadowed(sh, w, 5));

console.log("nestedLoops", nestedLoops({ x: 1 }, 4));
console.log("viaForEach", viaForEach({ x: 1 }, [1, 2, 3, 4]));
