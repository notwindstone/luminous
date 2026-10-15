// @native plus origin describe of hello points counters rest
// Classes written as class expressions bound to names (`var X = class
// {...}`, as bundlers and compilers emit class declarations): compiled as
// the class declarations they stand for, when nothing can tell the
// binding from one.

var Point = class {
  x: number;
  y: number;
  constructor(x: number, y: number) {
    this.x = x;
    this.y = y;
  }
  get length(): number {
    return Math.sqrt(this.x * this.x + this.y * this.y);
  }
  plus(o: InstanceType<typeof Point>): InstanceType<typeof Point> {
    return new Point(this.x + o.x, this.y + o.y);
  }
  static origin() {
    return new Point(0, 0);
  }
};

var Point3 = class extends Point {
  z: number;
  constructor(x: number, y: number, z: number) {
    super(x, y);
    this.z = z;
  }
  get length(): number {
    return Math.sqrt(super.length ** 2 + this.z * this.z);
  }
  describe() {
    return "Point3(" + this.x + ", " + this.y + ", " + this.z + ")";
  }
};

// Private names, accessors and arrow function fields.
let Counter = class Counter {
  #count = 0;
  step = 1;
  bump = () => {
    this.#count += this.step;
    return this;
  };
  get count() {
    return this.#count;
  }
  static of(n: number) {
    const c = new Counter();
    for (let i = 0; i < n; i++) c.bump();
    return c;
  }
};

const Named = class {
  static tag = "named";
  hello(who: string) {
    return "hello " + who + " from " + Named.tag;
  }
};

// Reached before its declaration runs (seen undefined): left to the
// interpreter, as a `var`.
function early() {
  return typeof Early === "undefined" ? "undefined" : "class";
}
const sawEarly = early();
var Early = class {
  who() {
    return "early";
  }
};

// Reassigned: left to the interpreter.
var Swapped = class {
  who() {
    return "first";
  }
};
const firstSwapped = new Swapped().who();
Swapped = class {
  who() {
    return "second";
  }
};

function points(): string {
  const p = new Point(3, 4);
  const q = Point.origin().plus(p);
  const r = new Point3(1, 2, 2);
  return [p.length, q.x, q.y, r.length, r.describe(), r instanceof Point, Point.name, Point3.name, Object.keys(r).join(",")].join(" ");
}

function counters(): string {
  const c = Counter.of(5);
  c.step = 10;
  c.bump().bump();
  return [c.count, Counter.name, typeof c.bump, Object.keys(c).join(",")].join(" ");
}

function rest(): string {
  return [new Named().hello("me"), Named.name, sawEarly, new Early().who(), firstSwapped, new Swapped().who()].join(" | ");
}

console.log(points());
console.log(counters());
console.log(rest());
