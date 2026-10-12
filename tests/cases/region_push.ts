// @native isAlpha lex codes fill spans guarded written
// @interpreted patchPush restorePush addSetter removeSetter
// Speculative regions that append to arrays: object literals of numbers
// (or numbers) pushed onto an array the region's guard found `push`
// appends to (a fast, extensible array with a fresh one's shape and
// `length` its element count, the builtin `push`, no index properties on
// the array prototypes) are stored directly, the array growing as needed.
// Arrays with other properties or holes, frozen or non-extensible ones,
// array-likes, a replaced `push` and an index setter on `Array.prototype`
// take the generic code, with the same results; so do elements of another
// shape midway through a loop that reads elements and appends. A region
// in `try`, or one that keeps a field's value in a local, does not append.

type Tok = { kind: number; start: number; end: number };

function isAlpha(c: number): boolean {
  return (c >= 97 && c <= 122) || (c >= 65 && c <= 90);
}

function lex(src: string): Tok[] {
  const out: Tok[] = [];
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src.charCodeAt(i);
    if (c === 32) {
      i++;
    } else if (isAlpha(c)) {
      const start = i;
      while (i < n && isAlpha(src.charCodeAt(i))) i++;
      out.push({ kind: 1, start, end: i });
    } else {
      out.push({ kind: 2, start: i, end: i + 1 });
      i++;
    }
  }
  return out;
}

// Numbers, fractions among them.
function codes(s: string, scale: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < s.length; i++) out.push(s.charCodeAt(i) * scale);
  return out;
}

// Into an array from elsewhere.
function fill(arr: Tok[], n: number): number {
  for (let i = 0; i < n; i++) arr.push({ kind: i % 3, start: i, end: i * 0.5 });
  return arr.length;
}

// Reads elements, and appends.
function spans(ts: Tok[]): number[] {
  const out: number[] = [];
  for (let i = 0; i < ts.length; i++) out.push(ts[i].end - ts[i].start);
  return out;
}

// In `try`.
function guarded(s: string): string {
  const out: number[] = [];
  try {
    for (let i = 0; i < s.length; i++) out.push(s.charCodeAt(i) + 1);
  } catch (e) {
    return "caught";
  }
  return out.join(",");
}

class Acc {
  total: number;
  constructor() {
    this.total = 0;
  }
}

// A field written as well.
function written(a: Acc, s: string): string {
  const out: number[] = [];
  for (let i = 0; i < s.length; i++) {
    a.total += s.charCodeAt(i);
    out.push(a.total);
  }
  return out.join(",") + " " + a.total;
}

let savedPush: unknown;
function patchPush(): void {
  "use no aot";
  savedPush = Array.prototype.push;
  Array.prototype.push = function (this: unknown[], ...xs: unknown[]): number {
    const f = savedPush as (...a: unknown[]) => number;
    return f.apply(this, xs.map((x) => (typeof x === "number" ? -x : x)));
  };
}
function restorePush(): void {
  "use no aot";
  Array.prototype.push = savedPush as typeof Array.prototype.push;
}
function addSetter(): void {
  "use no aot";
  Object.defineProperty(Array.prototype, "1", {
    set(v: unknown) {
      console.log("setter", typeof v);
    },
    configurable: true,
  });
}
function removeSetter(): void {
  "use no aot";
  delete (Array.prototype as unknown as Record<string, unknown>)["1"];
}

const show = (ts: Tok[]) => ts.map((t) => (t === undefined ? "_" : `${t.kind}:${t.start}-${t.end}`)).join(" ");
console.log(show(lex("let x1 = foo(bar) + 42;")));
console.log(codes("hello", 1).join(","), codes("ab", 0.5).join(","), codes("x".repeat(1000), 2).length);
const rope = "ab cd ".repeat(200) + "ef(g)".repeat(200);
const long = lex(rope);
console.log(long.length, show(long.slice(-4)));

const plain: Tok[] = [];
console.log(fill(plain, 5), show(plain));
const tagged: Tok[] = [];
(tagged as unknown as { tag: number }).tag = 1;
console.log(fill(tagged, 3), show(tagged));
const holes: Tok[] = [];
holes.length = 2;
console.log(fill(holes, 2), show(holes));
const frozen = Object.freeze([] as Tok[]) as Tok[];
try {
  fill(frozen, 2);
} catch (e) {
  console.log((e as Error).constructor.name, frozen.length);
}
const fixed = Object.preventExtensions([] as Tok[]);
try {
  fill(fixed, 2);
} catch (e) {
  console.log((e as Error).constructor.name, fixed.length);
}
const like = { length: 0, push: Array.prototype.push } as unknown as Tok[];
console.log(fill(like, 2), JSON.stringify(like));

const toks = lex("alpha (beta) gamma");
console.log(spans(toks).join(","));
const odd = [...toks] as unknown[];
odd[2] = { start: 1, end: 9, kind: 0 };
console.log(spans(odd as Tok[]).join(","));
console.log(guarded("abc"), written(new Acc(), "abc"));

patchPush();
console.log(codes("abc", 1).join(","), show(lex("ab c")), fill([], 2));
restorePush();
console.log(codes("abc", 1).join(","));
addSetter();
const withSetter = codes("xyz", 1);
console.log(withSetter.length, 1 in withSetter, withSetter.join(","));
removeSetter();
console.log(codes("xyz", 1).join(","), show(lex("a b")));
