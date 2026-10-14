// @native Point Counter Node Defaults Guarded Late Base WithSetter Empty OnlyFields
// @native make points counters list defaults inherit setters tdz extra
// @interpreted interpNew
// Constructors of base classes compiled natively: native `new` makes the
// instance with its fields (in declaration order, initializers first),
// then runs the body; interpreted code keeps the class's own constructor.

let log: string[] = [];

function note(s: string, v: number): number {
  log.push(s);
  return v;
}

function tdz(): string {
  try {
    return String(new Late(1).v);
  } catch (e) {
    return (e as Error).constructor.name;
  }
}
const early = tdz();

class Late {
  v: number;
  constructor(v: number) {
    this.v = v;
  }
}

class Point {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.y = y;
    this.x = x;
  }
  len(): number {
    return Math.sqrt(this.x * this.x + this.y * this.y);
  }
}

class Counter {
  count = note("count", 0);
  items: number[] = [];
  label = "c" + this_free();
  step;
  constructor(step: number) {
    log.push("body");
    this.step = step;
    this.bump();
  }
  bump(): void {
    this.count += this.step;
    this.items.push(this.count);
  }
}

function this_free(): string {
  return "!";
}

class Node {
  value: number;
  next: Node | null;
  constructor(value: number, depth: number) {
    this.value = value;
    this.next = depth > 0 ? new Node(value + 1, depth - 1) : null;
  }
}

class Defaults {
  a: number;
  b: string;
  constructor(a = 5, b = "x" + a) {
    this.a = a;
    this.b = b;
  }
}

class Guarded {
  ok = false;
  constructor(n: number) {
    if (n < 0) return;
    this.ok = true;
  }
}

class Base {
  tag = "base";
  constructor(public_: string) {
    (this as unknown as Record<string, string>).extra = public_;
  }
}

class Derived extends Base {
  more = 1;
  constructor() {
    super("d");
  }
}

class WithSetter {
  seen: string[] = [];
  constructor() {
    this.v = 3;
  }
  set v(x: number) {
    this.seen.push("set " + x);
  }
  get v(): number {
    return 42;
  }
}

class Hidden {
  #secret = 1;
  get secret(): number {
    return this.#secret;
  }
}

class Empty {}

class OnlyFields {
  a = 1;
  b = [2];
}

function make(): string {
  const p = new Point(3, 4);
  const q = new Point(1, 2);
  return [
    p.len(),
    Object.keys(p).join(","),
    JSON.stringify(q),
    p instanceof Point,
    p.constructor === Point,
    Object.getPrototypeOf(p) === Point.prototype,
    new Empty() instanceof Empty,
    JSON.stringify(new OnlyFields()),
    new Hidden().secret,
  ].join(" ");
}

function points(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const p = new Point(i, i + 1);
    s += p.x * 2 + p.y;
  }
  return s;
}

function counters(): string {
  log = [];
  const c = new Counter(3);
  c.bump();
  return [JSON.stringify(c), log.join(",")].join(" ");
}

function list(): string {
  const out: number[] = [];
  for (let n: Node | null = new Node(1, 4); n !== null; n = n.next) out.push(n.value);
  return out.join(",");
}

function defaults(): string {
  return [JSON.stringify(new Defaults()), JSON.stringify(new Defaults(2)), JSON.stringify(new Defaults(2, "y"))].join(
    " ",
  );
}

function inherit(): string {
  const d = new Derived();
  const b = new Base("b");
  return [JSON.stringify(d), JSON.stringify(b), d instanceof Base].join(" ");
}

function setters(): string {
  const w = new WithSetter();
  return [JSON.stringify(w), w.v, Object.keys(w).join(",")].join(" ");
}

function extra(): string {
  // More arguments than parameters (evaluated all the same).
  log = [];
  const p = new (Point as unknown as new (...a: number[]) => Point)(1, 2, note("third", 3));
  return JSON.stringify(p) + " " + log.join(",") + " " + new Guarded(-1).ok + " " + new Guarded(1).ok;
}

console.log(early);
console.log(make());
console.log(points(1000));
console.log(counters());
console.log(list());
console.log(defaults());
console.log(inherit());
console.log(setters());
console.log(extra());
console.log(tdz());
console.log(interpNew());

function interpNew(): string {
  "use no aot";
  // Interpreted construction makes the same objects.
  const p = new Point(5, 6);
  return JSON.stringify(p) + " " + Object.keys(p).join(",") + " " + JSON.stringify(new Counter(1));
}
