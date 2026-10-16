// @native buildDecorator buildContext counter makeShapes inLoop errors subclassing heritage describeClass main
// Classes inside functions: made natively each time their definition runs,
// their constructors native functions that `new` calls (from anywhere),
// their methods native closures over the function's variables.

// As Elysia makes the class of each request's context.
function buildDecorator(members: Record<string, unknown>) {
  class Decorator {}
  Object.assign(Decorator.prototype, members);
  return Decorator;
}

function buildContext(Base: any, headers: Record<string, string> | null = null) {
  const immutableHeaders = headers !== null && Object.isFrozen(headers);
  class Context extends Base {
    request: unknown;
    set: any;
    constructor(request: unknown) {
      super();
      this.request = request;
      if (immutableHeaders) this.set = { headers, status: undefined };
      else this.set = { headers: headers === null ? Object.create(null) : { ...headers }, status: undefined };
    }
  }
  return Context;
}

// Fields (one seeing the instance in an arrow function), methods, accessors
// and static methods over a variable of the function.
function counter(start: number) {
  let n = start;
  class Counter {
    step = 1;
    label: string;
    bump = () => this.next();
    constructor(label: string, step?: number) {
      this.label = label;
      if (step !== undefined) this.step = step;
    }
    next() {
      n += this.step;
      return this.label + n;
    }
    get value() {
      return n;
    }
    set value(v: number) {
      n = v;
    }
    static make() {
      return new Counter("made");
    }
    async later() {
      await null;
      return n;
    }
    *[Symbol.iterator]() {
      yield n;
    }
  }
  return Counter;
}

// Class expressions: named, anonymous, named by their binding; TS
// parameter properties; a getter-only accessor.
function makeShapes() {
  const Point = class {
    constructor(
      public x: number,
      public y: number = 0,
    ) {}
    get length() {
      return Math.hypot(this.x, this.y);
    }
    plus(o: { x: number; y: number }) {
      return new Point(this.x + o.x, this.y + o.y);
    }
  };
  const Named = class Inner {
    static self() {
      return Inner;
    }
  };
  const anonymous = [class {}][0];
  return { Point, Named, anonymous };
}

// A new class each time its definition runs, over that iteration's binding.
function inLoop(n: number) {
  const classes = [];
  for (let i = 0; i < n; i++) {
    classes.push(
      class Item {
        id = i;
        static index() {
          return i;
        }
      },
    );
  }
  return classes;
}

function errors() {
  class HttpError extends Error {
    constructor(
      message: string,
      public status: number,
    ) {
      super(message);
      this.name = "HttpError";
    }
  }
  class NotFound extends HttpError {
    constructor(what = "page") {
      super(what + " not found", 404);
    }
  }
  class Plain extends TypeError {}
  return { HttpError, NotFound, Plain };
}

function interpretedBase() {
  "use no aot";
  return class Base {
    kind: string;
    constructor(kind: string) {
      this.kind = kind;
    }
    hello() {
      return "hello " + this.kind;
    }
  };
}

function subclassing() {
  const Base = interpretedBase();
  class Local extends Base {
    extra: number;
    constructor(extra: number) {
      super("local");
      this.extra = extra;
    }
  }
  class Again extends Local {
    more = "more";
  }
  class Collection extends Map<string, number> {
    total() {
      let t = 0;
      for (const v of this.values()) t += v;
      return t;
    }
  }
  class List extends Array<number> {}
  return { Base, Local, Again, Collection, List };
}

function interpretedSub(Local: any) {
  "use no aot";
  return class Sub extends Local {
    sub = true;
  };
}

// What the class definition checks of its superclass.
function heritage(parent: any) {
  try {
    const C = class extends parent {};
    try {
      return "made " + typeof new C();
    } catch (e) {
      return "new: " + (e as Error).constructor.name + " " + (e as Error).message;
    }
  } catch (e) {
    return "class: " + (e as Error).constructor.name + " " + (e as Error).message;
  }
}

function describeClass(C: any): string {
  const d = (o: object, k: string) => Object.getOwnPropertyDescriptor(o, k)!;
  return [
    JSON.stringify(C.name),
    C.length,
    Object.getOwnPropertyNames(C).join(),
    Object.getOwnPropertyNames(C.prototype).join(),
    d(C, "prototype").writable,
    d(C, "prototype").enumerable,
    d(C.prototype, "constructor").enumerable,
    C.prototype.constructor === C,
    typeof C,
  ].join(" ");
}

function main(): void {
  const D = buildDecorator({ status: (x: number) => x * 2 });
  const C = buildContext(D);
  const frozen = Object.freeze({ a: "1" });
  const F = buildContext(D, frozen);
  const c: any = new C("req");
  const f: any = new F("req2");
  console.log(c.request, JSON.stringify(c.set), c.status(21), c instanceof C, c instanceof D, f.set.headers === frozen);
  console.log(describeClass(C), "|", describeClass(D));

  const K = counter(10);
  const k: any = new K("k");
  console.log(k.next(), k.bump(), k.value, (k.value = 3), k.next(), K.make().next(), k.step, new K("s", 5).next());
  console.log(describeClass(K), Object.keys(k).join(), [...k].join());
  k.later().then((v: number) => console.log("later", v));
  const desc = Object.getOwnPropertyDescriptor(K.prototype, "value")!;
  console.log(typeof desc.get, typeof desc.set, desc.enumerable, Object.getOwnPropertyDescriptor(K.prototype, "next")!.enumerable);
  try {
    (K as any)();
  } catch (e) {
    console.log((e as Error).constructor.name);
  }
  console.log(Reflect.construct(K, ["r"], D) instanceof D, new (counter(0))("other").next(), k.next());

  const { Point, Named, anonymous } = makeShapes();
  const p = new Point(3, 4);
  console.log(p.length, p.plus(new Point(1)).x, JSON.stringify(p), Point.name, Named.name, Named.self() === Named);
  console.log(JSON.stringify(anonymous.name), describeClass(anonymous));

  const items = inLoop(3);
  console.log(items.map((I) => new I().id + ":" + I.index()).join(), items[0] === items[1], items[0].name);

  const { HttpError, NotFound, Plain } = errors();
  const nf: any = new NotFound();
  console.log(nf.message, nf.status, nf.name, nf instanceof HttpError, nf instanceof Error, String(nf), typeof nf.stack);
  const pl = new Plain("p");
  console.log(pl instanceof TypeError, pl.name, pl.message, Object.prototype.toString.call(pl));

  const { Base, Local, Again, Collection, List } = subclassing();
  const l: any = new Local(1);
  const a: any = new Again(2);
  console.log(l.hello(), l.extra, a.hello(), a.extra, a.more, a instanceof Local, a instanceof Base);
  const Sub = interpretedSub(Local);
  const s = new Sub(3);
  console.log(s.sub, s.extra, s.kind, s instanceof Local);
  const col = new Collection([["a", 1], ["b", 2]]);
  console.log(col.total(), col.size, col instanceof Map);
  const list = new List();
  list.push(1, 2);
  console.log(list.length, Array.isArray(list), list instanceof List);

  console.log(heritage(D), "|", heritage(null), "|", heritage(42), "|", heritage(() => 1));
  const odd = function () {};
  odd.prototype = 5 as any;
  console.log(heritage(odd));

  console.log(String(K).replace(/\s+/g, " ").slice(0, 30), "|", String(K.prototype.next).replace(/\s+/g, " "));
  console.log(String(K.make).replace(/\s+/g, " "), "|", String(desc.get).replace(/\s+/g, " "));
}

main();
