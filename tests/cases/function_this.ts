// @native who getX Point Ret arrowThis nested counter defaults primitive match
// `this` in ordinary functions (strict: module code), against the generic answers.
const out = [];
function show(v) { return v === undefined ? "undefined" : v === null ? "null" : typeof v === "object" ? JSON.stringify(v) : String(v); }
function who() { return this; }
function getX(d) { return this === undefined ? "no this " + d : this.x + d; }
function Point(x, y) { this.x = x; this.y = y; }
Point.prototype.norm = function () { return Math.abs(this.x) + Math.abs(this.y); };
function Ret(v) { this.a = 1; return v; }
function arrowThis() { const f = () => this; return f(); }
function nested() { function inner() { return this; } return [inner(), inner.call(5)]; }
function counter() { this.n = (this.n || 0) + 1; return this.n; }
function defaults(a = this) { return a; }
function primitive() { return typeof this; }
// Hono's matcher: replaces itself on first use.
function match(method, path) {
  const table = this.build();
  const m = (method, path) => table[method + " " + path] ?? "none";
  this.match = m;
  return m(method, path);
}
class Router { constructor() { this.builds = 0; } build() { this.builds++; return { "GET /": "root", "GET /a": "a" }; } }
Router.prototype.match = match;

for (let i = 0; i < 3; i++) {
  out.push("direct " + show(who()) + " " + getX(1));
  const o = { x: 10, who, getX, counter };
  out.push("method " + show(o.who() === o) + " " + o.getX(2) + " " + o.counter() + " " + o.counter());
  out.push("call " + show(who.call(7)) + " " + getX.call({ x: 3 }, 4) + " " + show(who.apply(null)) + " " + getX.bind({ x: 5 })(6));
  const p = new Point(3, -4);
  out.push("new " + show(p) + " " + (p instanceof Point) + " " + p.norm() + " " + Object.getPrototypeOf(p) === Point.prototype);
  out.push("ret " + show(new Ret(1)) + " " + show(new Ret({ z: 2 })) + " " + show(new Ret(null)) + " " + show(Ret.call({}, 3)));
  out.push("arrow " + show(arrowThis()) + " " + show(arrowThis.call({ q: 1 })));
  out.push("nested " + show(nested()) + " " + show(nested.call({})));
  out.push("defaults " + show(defaults()) + " " + show(defaults.call({ d: 1 })) + " " + show(defaults.call({}, 9)));
  out.push("primitive " + primitive.call(1) + " " + primitive.call("s") + " " + primitive());
  const r = new Router();
  out.push("router " + r.match("GET", "/") + " " + r.match("GET", "/a") + " " + r.match("POST", "/") + " builds " + r.builds);
  const bound = r.match.bind(r);
  out.push("bound " + bound("GET", "/a"));
  let threw = "";
  try { getX.call(undefined, 1); new Point(1, 2).norm.call(undefined); } catch (e) { threw = e.constructor.name; }
  out.push("threw " + threw);
}
// As a constructor through a variable, with new.target's prototype.
const P = Point;
class Sub extends Point { constructor() { super(1, 2); this.z = 3; } }
const s = new Sub();
out.push("sub " + show(s) + " " + s.norm() + " " + (s instanceof Sub));
out.push("reflect " + show(Reflect.construct(Point, [5, 6], Ret)) + " " + (Object.getPrototypeOf(Reflect.construct(Point, [5, 6], Ret)) === Ret.prototype));
console.log(out.join("\n"));
