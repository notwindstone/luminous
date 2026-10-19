// @native addThree literalPlus nullDict compareAll protoKey nullProtoKey sideEffect loopAdds compound dupKeys readOnly
// @native escapes returned inherited nested deleted nullEscape selfRef
// Objects that never leave a function (`{}`, a literal, Object.create(null))
// whose properties are added after they are made keep them in locals: the
// same results as real objects, also when Object.prototype gets a setter
// or a read-only property of a key (the object is made then, with the
// properties it has, and the setter sees it), or Object.create is replaced;
// and where the object itself is needed (made there, once).
function errorOf(f: () => unknown): string {
  try {
    return "ok " + JSON.stringify(f());
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

function addThree(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const o: any = {};
    o.a = i;
    o.b = "b";
    o.c = 3;
    s += o.a + o.c + String(o.b).length;
  }
  return s;
}

function literalPlus(x: number): string {
  const o: any = { a: x, b: x * 2 };
  o.c = o.a + o.b;
  o.a = "first";
  o.d = o.c * 2;
  return o.a + " " + o.b + " " + o.c + " " + o.d;
}

function nullDict(x: number): string {
  const o: any = Object.create(null);
  o.k1 = x;
  o.k2 = o.k1 + 1;
  return [o.k1, o.k2, o.missing, o.toString, o.k3].join(",");
}

function compareAll(): string {
  const a: any = {};
  const b: any = Object.create(null);
  const r = [a === null, a !== null, a == undefined, a != null, null === a, undefined != a];
  const q = [b === null, b !== undefined, b == null, null != b];
  return r.join(",") + " " + q.join(",");
}

function protoKey(p: any): string {
  const o: any = {};
  o.x = 1;
  o.__proto__ = p;
  return String(o.__proto__ === p) + " " + o.x;
}

function nullProtoKey(): string {
  const o: any = Object.create(null);
  o.__proto__ = 5;
  return String(o.__proto__);
}

let log: string[] = [];
function sideEffect(): string {
  const o: any = {};
  o.first = 1;
  o.second = install();
  o.third = o.first + o.second;
  return o.first + " " + o.second + " " + o.third;
}

function install(): number {
  // A setter of `third` appears while the object is being made.
  Object.defineProperty(Object.prototype, "third", {
    set(v: any) {
      log.push("set third " + v + " on " + JSON.stringify(this));
    },
    configurable: true,
  });
  return 2;
}

function loopAdds(n: number): string {
  let out = "";
  for (let i = 0; i < n; i++) {
    const o: any = { i: i };
    if (i === 1) continue;
    o.sq = i * i;
    if (o.sq > 10) break;
    o.half = o.sq / 2;
    out += o.i + ":" + o.sq + ":" + o.half + " ";
  }
  return out;
}

function compound(): string {
  const o: any = {};
  o.n = 1;
  o.n += 5;
  o.n++;
  o.s = "x";
  o.s += o.n;
  return o.n + " " + o.s;
}

function dupKeys(): string {
  const o: any = { a: 1, b: 2, a: 3 };
  o.c = o.a;
  return o.a + " " + o.b + " " + o.c;
}

function readOnly(): string {
  const o: any = {};
  o.fine = 1;
  o.ro = 2;
  return "unreachable " + o.fine;
}

function escapes(x: number): string {
  const o: any = {};
  o.a = x;
  o.b = x + 1;
  const keys = Object.keys(o);
  o.c = 3;
  return keys.join(",") + " " + JSON.stringify(o) + " " + o.a;
}

function returned(x: number): any {
  const o: any = { kind: "point" };
  o.x = x;
  o.y = x * 2;
  return o;
}

function inherited(): string {
  const o: any = {};
  o.a = 1;
  return typeof o.toString + " " + o.a + " " + ("a" in o) + " " + o.hasOwnProperty("a");
}

function nested(flag: boolean): string {
  const o: any = {};
  o.a = 1;
  if (flag) o.b = 2;
  o.c = o.a + 1;
  return o.a + " " + o.b + " " + o.c + " " + JSON.stringify(o);
}

function deleted(): string {
  const o: any = { a: 1, b: 2 };
  o.c = 3;
  delete o.b;
  return JSON.stringify(o) + " " + o.b;
}

function nullEscape(): string {
  const o: any = Object.create(null);
  o.a = 1;
  o.b = "two";
  const p = Object.getPrototypeOf(o);
  return String(p) + " " + JSON.stringify(o) + " " + Object.keys(o).join("");
}

function selfRef(): string {
  const o: any = {};
  o.me = o;
  o.n = 1;
  return String(o.me === o) + " " + o.me.n;
}

console.log(addThree(10), addThree(1000));
console.log(escapes(1), JSON.stringify(returned(5)), inherited());
console.log(nested(true), nested(false), deleted());
console.log(nullEscape(), selfRef());
console.log(literalPlus(4), literalPlus(0.5));
console.log(nullDict(7), nullDict(-1));
console.log(compareAll());
const proto = { inherited: true };
console.log(protoKey(proto), protoKey(null));
console.log(nullProtoKey());
console.log(sideEffect(), log.join("; "));
log = [];
console.log(sideEffect(), log.join("; "));
delete (Object.prototype as any).third;
console.log(sideEffect(), log.join("; "));
delete (Object.prototype as any).third;
log = [];
console.log(loopAdds(10));
console.log(compound());
console.log(dupKeys());
console.log(errorOf(readOnly));
Object.defineProperty(Object.prototype, "ro", { value: 1, writable: false, configurable: true });
console.log(errorOf(readOnly));
delete (Object.prototype as any).ro;
console.log(errorOf(readOnly));
// A setter of a key added by a loop: called each time, with the object.
Object.defineProperty(Object.prototype, "b", {
  set(v: any) {
    log.push(JSON.stringify(this) + "<-" + v);
  },
  configurable: true,
});
console.log(addThree(3), log.join(" "));
delete (Object.prototype as any).b;
console.log(addThree(3));
// A getter only: assigning throws (strict).
Object.defineProperty(Object.prototype, "c", { get: () => 42, configurable: true });
console.log(errorOf(() => addThree(2)));
delete (Object.prototype as any).c;
// Object.create replaced: its result is used.
const create = Object.create;
(Object as any).create = (p: any) => ({ k3: "theirs", proto: p });
console.log(nullDict(1));
(Object as any).create = create;
console.log(nullDict(1), compareAll());
