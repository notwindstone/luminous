// @native bump moveAll viaLocal holes order wrongType chained compound
// Writes of fields of elements of arrays of an `aot type` (`xs[i].f = v`,
// `const p = xs[i]; p.f = v`): stores at the field's offset, with the
// value evaluated first, then what JS throws for an element that is
// `undefined`, then the field's type check.
type int = number;

aot type P = { x: int; y: number; tag: string };
aot type Q = { a: int } | { b: string };

const log: string[] = [];

function bump(ps: P[]): int {
  let s: int = 0;
  for (let i: int = 0; i < ps.length; i++) {
    ps[i].x = ps[i].x + 1;
    ps[i].y = ps[i].y * 0.5;
    s += ps[i].x | 0;
  }
  return s;
}

function moveAll(ps: P[], dx: int): void {
  for (let i: int = 0; i < ps.length; i++) ps[i].x = ps[i].x + dx;
}

function viaLocal(ps: P[]): string {
  for (let i: int = 0; i < ps.length; i++) {
    const p = ps[i];
    p.tag = p.tag + "!";
    p.x = 2147483647;
    p.x = p.x + 1; // wraps, as `| 0`
  }
  return ps.map((p) => p.tag + p.x).join(",");
}

function holes(ps: P[]): string {
  try {
    ps[5].x = 1;
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
  return "no error";
}

function rhs(n: int): int {
  log.push("rhs " + n);
  return n;
}

// The value is evaluated before the element is found missing.
function order(ps: P[]): string {
  try {
    ps[7].x = rhs(3);
  } catch (e) {
    log.push((e as Error).message);
  }
  const q = ps[9];
  try {
    q.y = rhs(4);
  } catch (e) {
    log.push((e as Error).message);
  }
  return log.join("; ");
}

function wrongType(ps: P[]): string {
  const out: string[] = [];
  try {
    (ps[0] as any).x = "seven";
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  try {
    ps[0].x = 1.5 as int;
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  out.push(String(ps[0].x));
  return out.join(" ");
}

function chained(ps: P[]): string {
  const v = (ps[1].y = 2.25);
  const w = (ps[1].x = 7);
  return v + " " + w + " " + ps[1].y + " " + ps[1].x;
}

function compound(qs: Q[]): string {
  for (let i: int = 0; i < qs.length; i++) {
    const q = qs[i];
    if ("a" in q) (q as { a: int }).a = (q as { a: int }).a * 3;
  }
  return JSON.stringify(qs);
}

const ps: P[] = [
  { x: 1, y: 8, tag: "a" },
  { x: 2, y: 4, tag: "b" },
  { x: 3, y: 2, tag: "c" },
];
console.log(bump(ps), JSON.stringify(ps));
moveAll(ps, 10);
console.log(JSON.stringify(ps));
console.log(viaLocal(ps));
console.log(holes(ps));
console.log(order(ps));
console.log(wrongType(ps));
console.log(chained(ps));
const qs: Q[] = [{ a: 1 }, { b: "x" }, { a: 5, b: "y" }];
console.log(compound(qs));
