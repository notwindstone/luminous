// @native fields aliased calls breaks exits exact
// Fields of `aot type` objects kept in locals across loops that run no
// other code, and integer arithmetic kept exact.

aot type V = { x: int; y: int; w: number };

let keep: V | null = null;

function fields(n: int): string {
  const p: V = { x: 1, y: 2, w: 0.5 };
  keep = p;
  for (let i = 0; i < n; i++) {
    p.x = (p.x + p.y * 3) & 1023;
    p.y = (p.y ^ i) & 255;
    p.w = p.w * 0.999 + p.x;
  }
  return JSON.stringify(p) + JSON.stringify(keep);
}

function aliased(n: int): string {
  // Two locals of one layout may be one object: nothing is kept.
  const p: V = { x: 0, y: 0, w: 0 };
  const q: V = n > 0 ? p : { x: 0, y: 0, w: 0 };
  for (let i = 0; i < n; i++) {
    p.x = p.x + 1;
    q.x = q.x + 10;
  }
  return JSON.stringify(p);
}

function bump(v: V): void {
  v.y = v.y + 100;
}

function calls(n: int): string {
  // A call may see the object: nothing is kept.
  const p: V = { x: 0, y: 0, w: 0 };
  keep = p;
  for (let i = 0; i < n; i++) {
    p.y = p.y + 1;
    bump(p);
  }
  return JSON.stringify(p);
}

function breaks(n: int): string {
  const p: V = { x: 0, y: 0, w: 1 };
  keep = p;
  outer: for (let i = 0; i < n; i++) {
    for (let j = 0; j < 4; j++) {
      p.x = p.x + j;
      if (p.x > 50) break outer;
      if (j === 2) continue;
      p.w = p.w * 2;
    }
    p.y = p.y + 1;
  }
  return JSON.stringify(p);
}

function exits(n: int): string {
  const p: V = { x: 0, y: 0, w: 0 };
  keep = p;
  let thrown = "";
  try {
    for (let i = 0; i < n; i++) {
      p.x = p.x + 1;
      if (i === 3) throw new Error("at " + p.x);
    }
  } catch (e) {
    thrown = (e as Error).message;
  }
  for (let i = 0; i < n; i++) {
    p.y = p.y + 2;
    if (i === 5) return JSON.stringify(p) + thrown;
  }
  return "unreached";
}

function exact(): string {
  const out: unknown[] = [];
  let a: int = 2147483647;
  let b: int = -2147483648;
  out.push(a * 3 + b, (a * 3 + b) & 0xffff, b - a, (b - a) | 0);
  const z: int = 0;
  out.push(Object.is(z * -3, -0), Object.is(z * 3, 0), Object.is(-1 * z, -0));
  // Past 2^53: rounded as doubles.
  out.push(a * 4194304 * 3 + 1, (a * 4194304 * 3 + 1) % 7);
  let s = 0;
  for (let i = 0; i < 5; i++) s += (i * 1000003 + a) & 1023;
  out.push(s);
  return out.join(",");
}

console.log(fields(1000));
console.log(aliased(3), aliased(0));
console.log(calls(3));
console.log(breaks(10));
console.log(exits(10));
console.log(exact());
