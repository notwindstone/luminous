// @native MyError AppError NotFound Plain Tagged Coded Cookie Sneaky Counted ListMap make main
// Classes extending `Error` (and other classes without a native
// constructor: built-ins, interpreted classes): native constructors whose
// `super(...)` constructs the superclass with `new.target` the class.
class MyError extends Error {
  code: number;
  constructor(message: string, code: number) {
    super(message);
    this.code = code;
  }
}

// As frameworks write them: a base error class, its subclasses, a name
// from the constructor, a cause.
var AppError = class extends Error {
  status?: number;
  constructor(message?: string, cause?: unknown) {
    super(message);
    this.name = this.constructor.name;
    if (cause) this.cause = cause;
  }
  toJSON() {
    return { name: this.name, message: this.message, status: this.status };
  }
};

var NotFound = class extends AppError {
  response: string;
  constructor(response = "Not Found") {
    super(response);
    this.response = response;
    this.status = 404;
  }
};

// Default constructors: all the arguments passed on.
class Plain extends Error {}
class Tagged extends TypeError {
  tag = "tagged";
}

// A field initializer that sees the instance; an options object.
class Coded extends RangeError {
  code = "E_" + this.message.toUpperCase();
  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
  }
}

// Defaults that run code before `super(...)`.
class Cookie extends AppError {
  key?: string;
  constructor(key?: string, response = key ? `"${key}" is an invalid cookie` : "Invalid Cookie", status?: number) {
    super(response);
    this.key = key;
    this.status = status ?? 400;
  }
}

class Other extends Error {
  constructor(m?: string) {
    super("other: " + m);
  }
}
let flip = false;
class Sneaky extends Cookie {
  constructor(m: string, x = flip ? Object.setPrototypeOf(Sneaky, Other) : 1) {
    super(m);
  }
}

// Other built-ins and an interpreted superclass.
class Counted extends Map<string, number> {
  hits = 0;
  constructor(entries?: [string, number][]) {
    super(entries);
  }
  bump(k: string): number {
    this.hits++;
    const v = (this.get(k) ?? 0) + 1;
    this.set(k, v);
    return v;
  }
}

class ListMap extends Array<number> {
  sum(): number {
    let s = 0;
    for (const x of this) s += x;
    return s;
  }
}

function makeBase() {
  // (A class made in a function: interpreted.)
  return class Base {
    kind: string;
    constructor(kind: string) {
      this.kind = kind;
    }
  };
}
const Base = makeBase();
function makeSub() {
  return class Sub extends MyError {
    extra = "sub";
  };
}
class FromInterpreted extends Base {
  extra: number;
  constructor(extra: number) {
    super("from " + extra);
    this.extra = extra;
  }
}

function describe(e: any): string {
  return [
    e.name,
    e.message,
    e instanceof Error,
    Object.prototype.toString.call(e),
    String(e),
    typeof e.stack,
    Object.keys(e).join(","),
  ].join(" | ");
}

function make(n: number): Error[] {
  return [
    new MyError("boom", n),
    new AppError("app", new Error("inner")),
    new NotFound(),
    new NotFound("gone"),
    new Plain("plain", { cause: n }),
    new Plain(),
    new Tagged("t"),
    new Coded("bad", { cause: "why" }),
    new Cookie(),
    new Cookie("sid", undefined, 401),
    new Sneaky("before"),
  ];
}

function main(): void {
  const errs = make(42);
  for (const e of errs) console.log(describe(e));
  const [my, app, nf, nf2, plain, empty, tagged, coded] = errs as any[];
  console.log(my.code, my instanceof MyError, Object.getPrototypeOf(my) === MyError.prototype);
  console.log(app.cause.message, JSON.stringify(app), "cause" in errs[2]);
  console.log(nf instanceof NotFound, nf instanceof AppError, nf.status, nf.response, nf2.message, JSON.stringify(nf2));
  console.log(plain.cause, "cause" in empty, empty.message === "", tagged instanceof TypeError, tagged.tag);
  console.log(coded.code, coded.cause, coded instanceof RangeError);
  try {
    throw new NotFound("thrown");
  } catch (e) {
    console.log("caught", (e as Error).name, (e as Error).message, e instanceof NotFound);
  }
  const c = new Counted([["a", 1]]);
  console.log(c.bump("a"), c.bump("b"), c.size, c.hits, c instanceof Map);
  const l = new ListMap();
  l.push(1, 2, 3);
  console.log(l.sum(), l.length, Array.isArray(l), l instanceof ListMap);
  const f = new FromInterpreted(7);
  console.log(f.kind, f.extra, f instanceof Base);
  flip = true;
  const sn: any = new Sneaky("after");
  console.log(sn.message, sn instanceof Other, sn instanceof Cookie, sn.status, describe(sn));
  // A superclass changed after the class was made: `super` sees it.
  Object.setPrototypeOf(Plain, RangeError);
  const p = new Plain("now a range error");
  console.log(p instanceof RangeError, p.name, String(p));
  // Subclassed by interpreted code.
  const Sub = makeSub();
  const s = new Sub("sub", 1);
  console.log(s.code, s.extra, s instanceof MyError, s.name);
}

main();
