// @native fromArrays holes growing subclass failures
// @interpreted patchIterator restoreIterator patchAdd restoreAdd patchNext restoreNext
// `new Set(array)` and `new Map(entries)` take an array's elements directly
// and call the builtin `add` / `set` without a call frame while the array
// has no other own property than `length`, Array.prototype[Symbol.iterator]
// and the array iterator's `next` are the builtins, and the collection's
// adder is the builtin: holes, a `length` past the elements, entries whose
// getters grow the array, a replaced iterator, `next` or adder, a subclass
// and adders that throw all behave as the language says.

function show(c: Set<unknown> | Map<unknown, unknown>): string {
  return [...c].map((x) => (Array.isArray(x) ? x.join(":") : String(x))).join(",") + "/" + c.size;
}

function fromArrays(): string {
  const words = ["a", "b", "a", "c", "b"];
  const s = new Set(words);
  const m = new Map<string, number>([["x", 1], ["y", 2], ["x", 3]]);
  const nums = new Set([1, 2, 2, NaN, NaN, -0, 0]);
  const empty = new Set([]);
  const objs = [{}, {}];
  const so = new Set([objs[0], objs[1], objs[0]]);
  return [show(s), show(m), show(nums), show(empty), String(so.size), show(new Map())].join(" ");
}

function holes(): string {
  // eslint-disable-next-line no-sparse-arrays
  const a = [1, , 3];
  const b = [4, 5];
  b.length = 4;
  const c = [6, 7];
  (c as unknown as { extra: number }).extra = 1;
  return [show(new Set(a)), show(new Set(b)), show(new Set(c))].join(" ");
}

function growing(): string {
  const entries: unknown[] = [];
  let n = 0;
  const grow = {
    get 0(): string {
      if (n++ < 2) entries.push(["k" + n, n]);
      return "g" + n;
    },
    1: "v",
  };
  entries.push(grow, ["a", 1]);
  return show(new Map(entries as [unknown, unknown][]));
}

class Tracked extends Set<number> {
  seen: number[] = [];
  add(v: number): this {
    if (this.seen) this.seen.push(v);
    return super.add(v * 10);
  }
}

class Plain extends Map<string, number> {}

function subclass(): string {
  const t = new Tracked([1, 2, 3]);
  const p = new Plain([["q", 1]]);
  return [show(t), String(t.seen), show(p), String(p instanceof Plain)].join(" ");
}

function failures(): string {
  const out: string[] = [];
  try {
    new Map([1, 2] as unknown as [unknown, unknown][]);
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  try {
    new WeakSet([{}, 1 as unknown as object]);
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  try {
    new Map([
      ["a", 1],
      {
        get 0(): string {
          throw new RangeError("entry");
        },
      } as unknown as [string, number],
    ]);
  } catch (e) {
    out.push((e as Error).message);
  }
  const ws = new WeakSet([out, failures]);
  out.push(String(ws.has(out)));
  return out.join(" ");
}

let savedIterator: unknown;
function patchIterator(): void {
  "use no aot";
  savedIterator = Array.prototype[Symbol.iterator];
  Array.prototype[Symbol.iterator] = function* (this: unknown[]) {
    for (let i = this.length - 1; i >= 0; i--) yield this[i];
  } as typeof Array.prototype.values;
}
function restoreIterator(): void {
  "use no aot";
  Array.prototype[Symbol.iterator] = savedIterator as typeof Array.prototype.values;
}
let savedAdd: unknown;
function patchAdd(): void {
  "use no aot";
  savedAdd = Set.prototype.add;
  Set.prototype.add = function (this: Set<unknown>, v: unknown) {
    return (savedAdd as (v: unknown) => Set<unknown>).call(this, "+" + String(v));
  } as typeof Set.prototype.add;
}
function restoreAdd(): void {
  "use no aot";
  Set.prototype.add = savedAdd as typeof Set.prototype.add;
}
let savedNext: unknown;
function patchNext(): void {
  "use no aot";
  const proto = Object.getPrototypeOf([][Symbol.iterator]());
  savedNext = proto.next;
  proto.next = function (this: Iterator<unknown>) {
    const r = (savedNext as () => IteratorResult<unknown>).call(this);
    return r.done ? r : { done: false, value: String(r.value) + "!" };
  };
}
function restoreNext(): void {
  "use no aot";
  Object.getPrototypeOf([][Symbol.iterator]()).next = savedNext;
}

console.log(fromArrays());
console.log(holes());
console.log(growing());
console.log(subclass());
console.log(failures());
patchIterator();
console.log(fromArrays());
restoreIterator();
patchAdd();
console.log(fromArrays());
restoreAdd();
patchNext();
console.log(holes());
restoreNext();
console.log(fromArrays());
