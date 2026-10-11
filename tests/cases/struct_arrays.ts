// @native creation holes stores methods iteration sure total norm calls
// Arrays declared `T[]` (T an `aot type`) made by literals and `new Array`
// hold only objects of T: native code reads their elements typed.

aot type P = { x: int; y: number };

function errorOf(f: () => void): string {
  try {
    f();
    return "ok";
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

function creation(): string {
  const a: P[] = [{ x: 1, y: 1.5 }, { x: 2, y: 2.5 }];
  const b: Array<P> = new Array(3);
  const c: P[] = Array(2);
  const d: P[] = new Array();
  const e: P[] = [];
  return [a.length, b.length, c.length, d.length, e.length, Array.isArray(a), JSON.stringify(a), JSON.stringify(b)].join("|");
}

function holes(): string {
  const a: P[] = new Array(2);
  a[0] = { x: 5, y: 6 };
  const out: unknown[] = [a[1] === undefined, a[9] === undefined, a[0].x, 0 in a, 1 in a];
  out.push(errorOf(() => a[1].x));
  out.push(errorOf(() => {
    const { x } = a[1];
    out.push(x);
  }));
  out.push(errorOf(() => a[-1].y));
  return out.join(",");
}

function stores(): string {
  const a: P[] = [];
  a[2] = { x: 3, y: 3 };
  a.push({ x: 4, y: 4 });
  const out: unknown[] = [a.length, JSON.stringify(a)];
  out.push(errorOf(() => (a as unknown[]).push({ x: 1 })));
  out.push(errorOf(() => ((a as unknown[])[0] = { x: 1, y: 1 })));
  out.push(errorOf(() => ((a as unknown[])[0] = 5)));
  out.push(errorOf(() => ((a as unknown as Record<string, unknown>).extra = 1)));
  out.push(errorOf(() => Object.defineProperty(a, 0, { get: () => 1 })));
  a[0] = undefined as unknown as P;
  delete a[2];
  out.push(JSON.stringify(a), Object.keys(a).join(","));
  a.length = 1;
  out.push(a.length, JSON.stringify(a));
  a.length = 3;
  out.push(a.length, errorOf(() => (a.length = -1)), errorOf(() => (a.length = 1.5)));
  return out.join(" ");
}

function methods(): string {
  const a: P[] = [{ x: 3, y: 0.5 }, { x: 1, y: 1.5 }, { x: 2, y: 2.5 }];
  const out: unknown[] = [];
  out.push(a.map((p) => p.x).join(","), a.filter((p) => p.x > 1).length, a.reduce((s, p) => s + p.y, 0));
  out.push(a.indexOf(a[1]), a.includes(a[2]), a.findIndex((p) => p.x === 2), a.some((p) => p.y > 2));
  a.sort((p, q) => p.x - q.x);
  out.push(JSON.stringify(a));
  a.reverse();
  out.push(a.map((p) => p.x).join(""), a.slice(1).length, Array.isArray(a.slice()), a.concat(a).length);
  const removed = a.splice(0, 1, { x: 9, y: 9 });
  out.push(removed.length, JSON.stringify(a));
  out.push(errorOf(() => (a as unknown[]).splice(0, 0, "no")));
  out.push(errorOf(() => (a as unknown[]).fill(0 as unknown as P)));
  a.unshift({ x: 0, y: 0 });
  a.fill({ x: 7, y: 7 }, 2);
  out.push(JSON.stringify(a));
  return out.join(" ");
}

function iteration(): string {
  const a: P[] = [{ x: 1, y: 2 }, { x: 3, y: 4 }];
  let s = 0;
  for (const p of a) s += p.x * p.y;
  const keys: string[] = [];
  for (const k in a) keys.push(k);
  return [s, keys.join(","), [...a].length, Object.entries(a).length].join(",");
}

// Only ever holds such arrays: reads are typed in every function.
let kept: P[] = [];

function sure(n: int): number {
  const a: P[] = new Array(n);
  for (let i = 0; i < n; i++) a[i] = { x: i, y: i / 2 };
  kept = a;
  let s = 0;
  for (let i = 0; i < kept.length; i++) {
    const { x, y } = kept[i];
    s += x * y;
  }
  return s;
}

// May hold a plain array: reads stay generic.
let loose: P[] = [];

function total(ps: P[]): number {
  let s = 0;
  for (let i = 0; i < ps.length; i++) s += ps[i].x;
  return s;
}

function norm(p: P): number {
  return p.x * p.x + p.y * p.y;
}

function calls(): string {
  const a: P[] = [{ x: 3, y: 4 }];
  const plain = JSON.parse('[{"x":3,"y":4}]');
  return [norm(a[0]), norm(plain[0]), total(a), total(plain), errorOf(() => norm(a[5]))].join(",");
}

console.log(creation());
console.log(holes());
console.log(stores());
console.log(methods());
console.log(iteration());
console.log(sure(10));
loose = JSON.parse('[{"x":1,"y":2}]');
console.log(total(loose), JSON.stringify(loose), Array.isArray(loose));
console.log(calls());
