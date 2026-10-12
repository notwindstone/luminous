// @interpreted installSetter removeSetter
// @native pushes pushObjects lengthened withProp frozenPush sealedPush subclassPush mapObjects filterObjects reassign keepRefs shrinking protoSetter
// Appending to arrays: `push` onto fresh arrays and `map` / `filter`
// results are appended inline; arrays with other shapes, frozen or
// non-extensible ones, a `length` past the elements, subclasses and index
// setters on Array.prototype take the engine's paths, with the same
// results. Callback parameters keep their element alive.

type P = { x: number; y: number };

function pushes(n: number): string {
  const xs: number[] = [];
  const lens: number[] = [];
  for (let i = 0; i < n; i++) lens.push(xs.push(i * 1.5));
  return xs.join(",") + " | " + lens.join(",");
}

function pushObjects(n: number): string {
  const xs: P[] = [];
  const names: string[] = [];
  for (let i = 0; i < n; i++) {
    const p = { x: i, y: -i };
    xs.push(p);
    xs.push(p);
    names.push("n" + i);
  }
  let s = 0;
  for (const p of xs) s += p.x - p.y;
  return s + " " + xs.length + " " + names.join("");
}

function lengthened(): string {
  const xs: number[] = [1, 2];
  xs.length = 5;
  const r = xs.push(9);
  return r + " " + xs.length + " " + JSON.stringify(xs);
}

function withProp(): string {
  const xs = [1, 2] as number[] & { tag?: string };
  xs.tag = "t";
  xs.push(3, 4);
  xs.push(5);
  return xs.join(",") + " " + xs.tag;
}

function frozenPush(): string {
  const xs: number[] = [1];
  Object.freeze(xs);
  try {
    xs.push(2);
  } catch (e) {
    return (e as Error).constructor.name + " " + xs.length;
  }
  return "no throw";
}

function sealedPush(): string {
  const xs: number[] = [1];
  Object.preventExtensions(xs);
  try {
    xs.push(2);
  } catch (e) {
    return (e as Error).constructor.name + " " + xs.length;
  }
  return "no throw";
}

class Stack extends Array<number> {
  top(): number {
    return this[this.length - 1];
  }
}

function subclassPush(): string {
  const s = new Stack();
  s.push(1);
  s.push(2);
  const doubled = s.map((v) => v * 2);
  return s.top() + " " + s.length + " " + (doubled instanceof Stack) + " " + doubled.join(",");
}

function mapObjects(ps: P[]): string {
  const moved = ps.map((p) => ({ x: p.x + 1, y: p.y * 2 }));
  const same = ps.map((p) => p);
  return JSON.stringify(moved) + " " + (same[0] === ps[0]) + " " + same.length;
}

function filterObjects(ps: P[]): string {
  const big = ps.filter((p) => p.x > 1);
  const none = ps.filter((p) => p.x > 100);
  big.push({ x: 0, y: 0 });
  return JSON.stringify(big) + " " + none.length;
}

function reassign(ps: P[]): string {
  const out = ps.map((p) => {
    const old = p;
    p = { x: old.x * 10, y: 0 };
    return p.x + old.y;
  });
  return out.join(",");
}

function keepRefs(ps: P[]): string {
  const kept: P[] = [];
  ps.forEach((p) => {
    if (p.x % 2 === 0) kept.push(p);
  });
  return kept.map((p) => p.x).join(",");
}

function shrinking(): string {
  const xs: P[] = [{ x: 1, y: 1 }, { x: 2, y: 2 }, { x: 3, y: 3 }, { x: 4, y: 4 }];
  const seen: number[] = [];
  xs.forEach((p) => {
    xs.pop();
    seen.push(p.x + p.y);
  });
  return seen.join(",") + " " + xs.length;
}

let setterCalls = 0;
function installSetter(): void {
  "use no aot";
  Object.defineProperty(Array.prototype, "2", {
    set(v: number) {
      setterCalls += v;
    },
    configurable: true,
  });
}

function removeSetter(): void {
  "use no aot";
  delete (Array.prototype as unknown as Record<string, unknown>)["2"];
}

function protoSetter(): string {
  installSetter();
  const xs: number[] = [1, 2];
  const r = xs.push(7);
  const ys = [1, 2, 3].map((v) => v + 1);
  removeSetter();
  return r + " " + xs.length + " " + JSON.stringify(xs) + " " + ys.join(",") + " " + setterCalls;
}

const ps: P[] = [{ x: 1, y: 2 }, { x: 2, y: 3 }, { x: 3, y: 4 }];
console.log(pushes(6));
console.log(pushObjects(4));
console.log(lengthened());
console.log(withProp());
console.log(frozenPush(), sealedPush());
console.log(subclassPush());
console.log(mapObjects(ps));
console.log(filterObjects(ps));
console.log(reassign(ps));
console.log(keepRefs([{ x: 2, y: 0 }, { x: 3, y: 0 }, { x: 4, y: 0 }]));
console.log(shrinking());
console.log(protoSetter());
