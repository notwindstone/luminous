// @native rounds skipped labeled captured hoisted nested iterate guarded doWhile gen slowSum
// What a loop body declares (`const`, `let`) is released when its
// iteration ends: each iteration gets its own value, `continue` before the
// declaration, `break`, labels, closures that keep a binding, `var`s that
// outlive the iteration, generators and async functions see the same
// values as before.

type Item = { id: number; tags: string[] };

function make(i: number): Item {
  return { id: i, tags: ["t" + i, "u" + (i % 3)] };
}

function rounds(n: number): string {
  const out: number[] = [];
  for (let r = 0; r < n; r++) {
    const items = [make(r), make(r + 1), make(r + 2)];
    let sum = 0;
    for (const it of items) sum += it.id + it.tags.length;
    out.push(sum);
  }
  return out.join(",");
}

function skipped(n: number): string {
  const out: string[] = [];
  for (let i = 0; i < n; i++) {
    if (i % 2 === 0) continue;
    const label = "odd" + i;
    const obj = { label, twice: label + label };
    if (i > 6) break;
    out.push(obj.twice);
  }
  return out.join(" ");
}

function labeled(): string {
  const out: string[] = [];
  outer: for (let i = 0; i < 4; i++) {
    const row = { i, cells: [] as string[] };
    for (let j = 0; j < 4; j++) {
      const cell = "c" + i + j;
      if (j === i) continue outer;
      if (i === 3 && j === 1) break outer;
      row.cells.push(cell);
      out.push(row.cells.join("") + "/" + row.i);
    }
  }
  return out.join(" ");
}

function captured(): string {
  const fns: (() => string)[] = [];
  for (let i = 0; i < 3; i++) {
    const tag = { name: "f" + i };
    fns.push(() => tag.name);
  }
  return fns.map((f) => f()).join(",");
}

function hoisted(): string {
  const out: string[] = [];
  for (let i = 0; i < 3; i++) {
    if (i > 0) out.push(String(prev));
    var prev = { i };
    out.push(JSON.stringify(prev));
  }
  return out.join(" ") + " " + JSON.stringify(prev);
}

function nested(n: number): number {
  let total = 0;
  let i = 0;
  while (i < n) {
    const outerObj = { v: i };
    let j = 0;
    while (j < 3) {
      const inner = { w: outerObj.v * 10 + j };
      total += inner.w;
      j++;
    }
    i++;
  }
  return total;
}

function iterate(o: Record<string, Item>): string {
  const out: string[] = [];
  for (const k in o) {
    const it = o[k];
    const desc = k + ":" + it.tags.join("+");
    out.push(desc);
  }
  return out.join(" ");
}

function guarded(n: number): string {
  const out: string[] = [];
  for (let i = 0; i < n; i++) {
    const res = { i, note: "" };
    try {
      if (i === 2) throw new Error("two");
      res.note = "ok";
    } catch (e) {
      const msg = (e as Error).message;
      res.note = msg;
    } finally {
      const done = "done" + i;
      out.push(res.note + "/" + done);
    }
  }
  return out.join(" ");
}

function doWhile(n: number): number {
  let i = 0;
  let s = 0;
  do {
    const box = { v: i * 2 };
    s += box.v;
    i++;
  } while (i < n);
  return s;
}

function* gen(n: number): Generator<string> {
  for (let i = 0; i < n; i++) {
    const v = { s: "g" + i };
    yield v.s;
    const after = v.s + "!";
    yield after;
  }
}

async function slowSum(n: number): Promise<number> {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const part = { v: await Promise.resolve(i) };
    s += part.v;
  }
  return s;
}

console.log(rounds(5));
console.log(skipped(10));
console.log(labeled());
console.log(captured());
console.log(hoisted());
console.log(nested(4));
console.log(iterate({ a: make(1), b: make(2) }));
console.log(guarded(4));
console.log(doWhile(5), doWhile(0));
console.log([...gen(3)].join(","));
slowSum(5).then((s) => console.log("async", s));
