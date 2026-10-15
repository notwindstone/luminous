// @native locals props privates elements typed named awaited assigned
// Logical assignment (`??=`, `||=`, `&&=`): the target is evaluated once and
// read; the right-hand side runs, and is assigned, only when needed.

let log: string[] = [];
function note<T>(s: string, v: T): T {
  log.push(s);
  return v;
}

let moduleCount: number | undefined;

function locals(): string {
  let a: number | null = null;
  let b: string | undefined = "kept";
  let c = 0;
  let d = 5;
  let e: unknown = "";
  const r1 = (a ??= note("a", 1));
  const r2 = (b ??= note("b", "new"));
  const r3 = (c ||= note("c", 7));
  const r4 = (d ||= note("d", 9));
  const r5 = (d &&= note("d2", 11));
  const r6 = (e &&= note("e", "no"));
  moduleCount ??= 3;
  moduleCount ??= 4;
  // Captured by a closure.
  let shared: string | undefined;
  const get = () => (shared ??= "made once");
  get();
  shared = undefined;
  const again = get();
  const out = [a, b, c, d, e, r1, r2, r3, r4, r5, r6, moduleCount, again, log.join(",")].join(" ");
  log = [];
  return out;
}

class Box {
  #inner: number | undefined;
  value: number | null = null;
  fill() {
    this.#inner ??= note("inner", 42);
    this.#inner ??= note("inner2", 0);
    return this.#inner;
  }
}

function props(): string {
  const o: Record<string, any> = { x: 0, y: "y", z: null };
  let keys = 0;
  const key = (k: string) => {
    keys++;
    return k;
  };
  o[key("x")] ||= note("x", 1);
  o[key("y")] ||= note("y", 2);
  o[key("z")] ??= note("z", 3);
  o[key("w")] &&= note("w", 4);
  o.v ??= {};
  o.v.deep ??= [];
  o.v.deep.push(1);
  // The object is evaluated once; reassigning it in the value does not
  // change where the value goes.
  let target: Record<string, any> = { p: undefined };
  const first = target;
  target.p ??= ((target = { p: "other" }), "set");
  const b = new Box();
  b.value ??= 7;
  const out = [JSON.stringify(o), keys, first.p, target.p, b.value, log.join(",")].join(" ");
  log = [];
  return out;
}

function privates(): string {
  const b = new Box();
  return b.fill() + " " + b.fill() + " " + log.splice(0).join(",");
}

function elements(): string {
  const xs: (number | null)[] = [0, null, 2];
  let i = 0;
  xs[i++] ||= 10;
  xs[i++] ??= 20;
  xs[i++] &&= 30;
  return xs.join(",") + " " + i;
}

function typed(): string {
  let n = 3;
  n ??= note("never", 0);
  let ok = true;
  ok &&= false;
  let s = "";
  s ||= "filled";
  return [n, ok, s, log.splice(0).join(",")].join(" ");
}

function named(): string {
  let f: (() => number) | undefined;
  f ??= () => 1;
  let g: any = 0;
  g ||= function () {
    return 2;
  };
  return f.name + " " + g.name + " " + f() + g();
}

async function awaited(): Promise<string> {
  let a: number | undefined;
  let b = 1;
  const o: Record<string, any> = {};
  a ??= await Promise.resolve(5);
  b ||= await note("not awaited", Promise.resolve(6));
  o.k ??= await Promise.resolve("k");
  o.k ??= await note("not either", Promise.resolve("no"));
  const r = (o.m ||= await Promise.resolve("m"));
  return [a, b, o.k, o.m, r, log.splice(0).join(",")].join(" ");
}

console.log(locals());
console.log(props());
console.log(privates());
console.log(elements());
console.log(typed());
console.log(named());
awaited().then(console.log);

// (Plain assignment names anonymous functions too.)
function assigned(): string {
  let f: any;
  f = () => 1;
  let g: any;
  g = function () {};
  const o: any = {};
  o.h = () => 2;
  return f.name + "|" + g.name + "|" + o.h.name;
}
console.log(assigned());
