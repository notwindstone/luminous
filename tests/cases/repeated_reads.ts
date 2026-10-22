// @native weight pick pickAny refresh sameArgs argWritten cellRead viaCell elemsRead viaElems getterBetween setterBetween coercedBetween readAfterWrite readAcross guardedTwice throwsFirst branchWrites
// A property read, or a call of a function that writes and allocates
// nothing, made by a statement and again by the code after it (later
// statements, the branches of the `if` whose test made it) is made once
// (effect summaries; see src/reuse.rs), and still gives what making it
// again would: across writes of what the callee reads (an argument's
// field, a module variable, array elements), getters, setters and
// coercions in between, and a callee that throws.
type Pt = { x: number; y: number };

// Too large to be inlined; reads its arguments only.
function weight(a: number, b: number): number {
  let s = 0;
  for (let k = 0; k < 4; k++) {
    const t = Math.abs(a - b * k) % 7;
    s += t > 3 ? t * 2 - 1 : t + 0.5;
    if (s > 1000) s = s / 3;
  }
  return s + (a > b ? 1 : 0) + (a === b ? 2 : 0) + (b < 0 ? 3 : 0);
}
function pick(a: number, b: number, limit: number): number {
  if (weight(a, b) > limit) return weight(a, b) * 2;
  return -weight(a, b);
}
// Arguments checked as the call is made: a value that is no number calls
// the function as interpreted, which converts it (each time).
function pickAny(a: any, b: any): number {
  if (weight(a, b) > 3) return weight(a, b);
  return -weight(a, b);
}
function refresh(n: number): number {
  return n + 1;
}
function sameArgs(a: number, b: number): number {
  const first = weight(a, b);
  const r = refresh(a);
  return first + weight(a, b) + r;
}
function argWritten(a: number, b: number): number {
  const first = weight(a, b);
  a = a + 1;
  return first + weight(a, b);
}

// A callee reading a module variable, written in between.
let level = 2;
function cellRead(k: number): number {
  let s = 0;
  for (let i = 0; i < 3; i++) s += k * level + i;
  return s + (k > 5 ? 1 : 0) + (k === 3 ? 2 : 0) + (level > 9 ? 4 : 0);
}
function viaCell(k: number): number {
  const first = cellRead(k);
  level = level + 5;
  return first + cellRead(k);
}

// A callee reading array elements, written in between.
function elemsRead(xs: number[], i: number): number {
  let s = 0;
  for (let j = 0; j < 3; j++) s += xs[(i + j) % xs.length] * (j + 1);
  return s + (i > 5 ? 1 : 0) + (xs.length > 9 ? 2 : 0) + (s > 100 ? 3 : 0);
}
function viaElems(xs: number[]): number {
  const first = elemsRead(xs, 0);
  xs[1] = 100;
  return first + elemsRead(xs, 0);
}

// Reads of a JS value's properties around calls and code the engine runs.
function getterBetween(o: any, g: any): number {
  const first = o.x;
  const t = g.tick;
  return first + o.x + t;
}
function setterBetween(o: any, s: any): number {
  const first = o.x;
  s.poke = o;
  return first + o.x;
}
function coercedBetween(o: any, k: any): number {
  const first = o.x;
  const t = 1 * k;
  return first + o.x + t;
}
function readAfterWrite(o: any, q: any): number {
  const first = o.x;
  q.x = 50;
  return first + o.x;
}
function bump(q: any): number {
  q.y = q.y + 1;
  return q.y;
}
function readAcross(o: any): number {
  const first = o.x;
  const b = bump(o);
  return first + o.x + b;
}
function guardedTwice(o: any): number {
  if (o.x > 1) return o.x * 10;
  return o.x;
}
function throwsFirst(o: any): number {
  let r = 0;
  try {
    r = o.x;
  } catch (e) {
    r = -1;
  }
  return r + (o ? o.x : 0);
}
function branchWrites(o: any): number {
  if (o.x > 0) {
    o.x = 7;
    return o.x;
  }
  return o.x;
}

console.log("pick", pick(3, 1, 2), pick(1, 3, 100), pick(2, 2, 3));
let conversions = 0;
const counted = {
  valueOf() {
    return ++conversions * 3;
  },
};
console.log("pickAny", pickAny(5, 1), pickAny(counted, 1), pickAny(counted, 1), conversions);
console.log("sameArgs", sameArgs(4, 1), "argWritten", argWritten(4, 1));
console.log("viaCell", viaCell(3), level);
console.log("viaElems", viaElems([1, 2, 3, 4]));

let ticks = 0;
const ticker = {
  get tick() {
    shared.x += 100;
    return ++ticks;
  },
};
const shared: any = { x: 1 };
console.log("getterBetween", getterBetween(shared, ticker), shared.x);
const poker = {
  set poke(o: any) {
    o.x = -5;
  },
};
console.log("setterBetween", setterBetween({ x: 3 }, poker));
const victim: any = { x: 2 };
const k = {
  valueOf() {
    victim.x = 40;
    return 3;
  },
};
console.log("coercedBetween", coercedBetween(victim, k));
const same: any = { x: 1 };
console.log("readAfterWrite", readAfterWrite(same, same), readAfterWrite({ x: 1 }, { x: 0 }));
console.log("readAcross", readAcross({ x: 2, y: 5 }));
let reads = 0;
const counting = {
  get x() {
    return ++reads;
  },
};
console.log("guardedTwice", guardedTwice({ x: 3 }), guardedTwice(counting), reads);
console.log("throwsFirst", throwsFirst({ x: 4 }), throwsFirst(null));
console.log("branchWrites", branchWrites({ x: 1 }), branchWrites({ x: -1 }));
