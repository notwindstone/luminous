// @native count states guarded delegate twoWay maker defaults reentrant consume run
const log: string[] = [];
const show = (r: IteratorResult<unknown>) => JSON.stringify(r);

function* count(n: number): Generator<number, string> {
  for (let i = 0; i < n; i++) yield i;
  return "end";
}

function states(): string {
  const out: string[] = [];
  const a = count(2);
  out.push(show(a.return("early" as any)), show(a.next()));
  const b = count(2);
  try {
    b.throw(new Error("at start"));
  } catch (e) {
    out.push("thrown " + (e as Error).message, show(b.next()));
  }
  const c = count(3);
  out.push(show(c.next()), show(c.return("mid" as any)), show(c.next()));
  const d = count(1);
  out.push(show(d.next()), show(d.next()), show(d.next()), show(d.return(5 as any)));
  return out.join(" ");
}

function* guarded(): Generator<string, string, string> {
  try {
    const got = yield "first";
    log.push("got " + got);
    yield "second";
    return "done";
  } catch (e) {
    log.push("caught " + (e as Error).message);
    return "recovered";
  } finally {
    log.push("finally");
    const extra = yield "from finally";
    log.push("extra " + extra);
  }
}

function* delegate(inner: Iterable<unknown>): Generator<unknown, string> {
  const r = yield* inner;
  log.push("delegate result " + String(r));
  return "outer";
}

function* twoWay(): Generator<number, void, number> {
  let total = 0;
  while (true) {
    const v = yield total;
    if (v === undefined) break;
    total += v;
  }
}

class Tree {
  constructor(public value: number, public children: Tree[] = []) {}
  *walk(): Generator<number> {
    yield this.value;
    for (const c of this.children) yield* c.walk();
  }
  static *range(a: number, b: number) {
    for (let i = a; i < b; i++) yield i;
  }
}

function maker(prefix: string) {
  return function* (n: number) {
    for (let i = 0; i < n; i++) yield prefix + i;
  };
}

let defaultsEvaluated = 0;
function* defaults(a: number = ++defaultsEvaluated, { b } = { b: 2 }) {
  yield a + b;
}

function* reentrant(): Generator<string> {
  try {
    yield (self as any).next();
  } catch (e) {
    yield "inner error: " + (e as Error).message;
  }
}
let self: Generator<string>;

function consume(): string {
  const out: unknown[] = [];
  for (const x of count(10)) {
    if (x === 2) break;
    out.push(x);
  }
  const [a, b, ...rest] = count(5);
  out.push(a, b, rest.length);
  const gen = guarded();
  for (const x of gen) {
    out.push(x);
    break;
  }
  return out.join(",");
}

function run(): string {
  const out: string[] = [];
  const g = guarded();
  out.push(show(g.next()), show(g.next("A")), show(g.next()), show(g.next("B")), show(g.next()));
  const h = guarded();
  out.push(show(h.next()), show(h.throw(new Error("boom"))), show(h.next("C")), show(h.next()));
  const r = guarded();
  out.push(show(r.next()), show(r.return("R")), show(r.next("D")), show(r.next()));
  return out.join("\n");
}

console.log(states());
console.log(run());
console.log(log.splice(0).join(" | "));

// yield*: values, the result, and forwarding of next / throw / return.
const d1 = delegate(count(2));
console.log(show(d1.next()), show(d1.next()), show(d1.next()), log.splice(0).join(" | "));
const d2 = delegate(guarded());
console.log(show(d2.next()), show(d2.next("x")), show(d2.throw(new Error("fwd"))), show(d2.next()), show(d2.next()));
console.log(log.splice(0).join(" | "));
const d3 = delegate(guarded());
console.log(show(d3.next()), show(d3.return("ret")), show(d3.next("y")), show(d3.next()));
console.log(log.splice(0).join(" | "));
const noThrow = { [Symbol.iterator]() { return { next: () => ({ done: false, value: 1 }), return: () => { log.push("closed"); return {}; } }; } };
const d4 = delegate(noThrow);
d4.next();
try {
  d4.throw(new Error("x"));
} catch (e) {
  console.log((e as Error).name, (e as Error).message, log.splice(0).join(" | "));
}
const d5 = delegate([7, 8]);
console.log(show(d5.next()), show(d5.return("no return method")), show(d5.next()));

const t = twoWay();
console.log(show(t.next()), show(t.next(5)), show(t.next(10)), show(t.next()));
const tree = new Tree(1, [new Tree(2, [new Tree(3)]), new Tree(4)]);
console.log([...tree.walk()], [...Tree.range(2, 5)], Object.getPrototypeOf(Tree.range(0, 1)) === Tree.range.prototype);
const letters = maker("x");
console.log([...letters(3)], Object.prototype.toString.call(letters), typeof letters.prototype);
const gd = defaults();
console.log(defaultsEvaluated, show(gd.next()), defaultsEvaluated, [...defaults(10, { b: 1 })]);
self = reentrant();
console.log(show(self.next()));
console.log(consume(), log.splice(0).join(" | "));
