// @native basics typed holes mutate throwing patched notArrays nested found withThis
// @interpreted patchMap
// Native loops for array callbacks (`forEach`, `map`, `filter`, `some`,
// `every`, `find`, `findIndex` with an arrow function inline): captured
// variables, typed parameters, holes, arrays changed by the callback,
// exceptions, replaced builtins, array-likes and subclasses.

type P = { x: number; y: number };

function points(n: number): P[] {
  const ps: P[] = [];
  for (let i = 0; i < n; i++) ps.push({ x: i % 7, y: i % 5 });
  return ps;
}

function basics(n: number): string {
  const ps = points(n);
  const dx = 3;
  let calls = 0;
  let s = 0;
  const moved = ps.map((p) => ({ x: p.x + dx, y: p.y - 1 }));
  const near = moved.filter((p) => p.x > 5);
  moved.forEach((p, i) => {
    s += p.x * i;
    calls++;
  });
  return [
    moved.length,
    near.length,
    JSON.stringify(near.slice(0, 2)),
    s,
    calls,
    ps.some((p) => p.x === 6),
    ps.some((p) => p.x === 60),
    ps.every((p) => p.y < 5),
    ps.every((p) => p.y < 4),
    JSON.stringify(ps.find((p) => p.x + p.y === 9)),
    String(ps.find((p) => p.x > 100)),
    ps.findIndex((p) => p.x + p.y === 9),
    ps.findIndex((p) => p.x > 100),
    [].map((x) => x).length,
    ps.map((p, i, arr) => arr.length - i + p.x).join(","),
  ].join(" ");
}

function typed(): string {
  const nums = [1, 2, 3, 4, 5];
  const sq = nums.map((x: number) => x * x);
  const idx = nums.map((x: number, i: number) => x * 10 + i);
  // Elements that are not numbers: the callback is called instead.
  const mixed = [1, "a", 3, null, undefined, 2.5] as unknown as number[];
  const doubled = mixed.map((x: number) => x * 2);
  const kept = mixed.filter((x: number) => x > 1);
  return [sq.join(","), idx.join(","), String(doubled), String(kept)].join(" ");
}

function holes(): string {
  const a = [1, , 3] as number[];
  const b = a.map((x) => x * 10);
  let seen = 0;
  a.forEach(() => seen++);
  const tail: number[] = [1, 2];
  tail.length = 4;
  return [JSON.stringify(b), 1 in b, b.length, seen, tail.map((x) => x + 1).length, tail.some((x) => x === undefined)].join(
    " ",
  );
}

function mutate(): string {
  const c: number[] = [];
  [5, 6, 7].forEach((x, i, arr) => {
    if (i === 0) arr.push(8);
    c.push(x);
  });
  const d = [1, 2, 3, 4];
  const e = d.map((x, i) => {
    if (i === 1) d.length = 2;
    return x;
  });
  const f = [1, 2, 3, 4, 5];
  const g = f.filter((x, i) => {
    if (i === 0) f.pop();
    return x % 2 === 1;
  });
  const h = [1, 2, 3];
  const k = h.map((x) => {
    h[2] = 30;
    return x;
  });
  return [c.join(","), JSON.stringify(e), d.length, g.join(","), k.join(",")].join(" ");
}

function throwing(): string {
  const out: string[] = [];
  let n = 0;
  try {
    [1, 2, 3].forEach((x) => {
      n++;
      if (x === 2) throw new Error("at " + x);
    });
  } catch (e) {
    out.push((e as Error).message + " after " + n);
  }
  try {
    points(5).map((p) => {
      if (p.x === 3) throw new RangeError("three");
      return p;
    });
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  return out.join(", ");
}

function patchMap(): () => void {
  "use no aot";
  const orig = Array.prototype.map;
  (Array.prototype as unknown as { map: unknown }).map = function (this: number[], f: (x: number) => number) {
    return orig.call(this, (x: number) => f(x) + 1000);
  };
  return () => {
    Array.prototype.map = orig;
  };
}

function patched(): string {
  const xs = [1, 2, 3];
  const restore = patchMap();
  const r = xs.map((x) => x * 2);
  restore();
  return r.join(",") + " " + xs.map((x) => x * 2).join(",");
}

class Mine extends Array<number> {}

function notArrays(): string {
  const like = {
    map(f: (x: number) => number): number[] {
      return [f(10), f(20)];
    },
  };
  const ta = new Int32Array([3, 4]);
  const set = new Set([1, 2]);
  const m = Mine.from([1, 2, 3]) as Mine;
  return [
    (like as unknown as number[]).map((x) => x + 1).join(","),
    Array.from(ta.map((x) => x * 3)).join(","),
    [...set].map((x) => x + 1).join(","),
    m.map((x) => x + 1) instanceof Mine,
    m.filter((x) => x > 1).length,
  ].join(" ");
}

function nested(): string {
  const rows = [
    [1, 2],
    [3, 4],
  ];
  const out: number[] = [];
  rows.forEach((row) => {
    out.push(row.map((x) => x * 10).reduce((a, b) => a + b, 0));
  });
  const flat = rows.map((row) => row.filter((x) => x % 2 === 0).length);
  return out.join(",") + " " + flat.join(",");
}

function found(): string {
  // `find` returns the element itself.
  const ps = points(10);
  const p = ps.find((q) => q.x === 4)!;
  p.y = 99;
  return JSON.stringify(ps[4]) + " " + (ps.find((q) => q.y === 99) === p);
}

function withThis(): string {
  // A `thisArg`: called, not inlined.
  const ctx = { k: 3 };
  return [1, 2].map((x) => x * ctx.k, ctx).join(",");
}

console.log(basics(20));
console.log(typed());
console.log(holes());
console.log(mutate());
console.log(throwing());
console.log(patched());
console.log(notArrays());
console.log(nested());
console.log(found());
console.log(withThis());
