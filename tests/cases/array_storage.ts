// @native lit
// Arrays whose elements start in their own cell: growing, shrinking,
// becoming slow arrays, being sorted, spliced, and collected.
const out = [];
function lit(i) { return [i, "s" + i, { v: i }, [i]]; }
let keep = [];
for (let r = 0; r < 300; r++) {
  const a = lit(r);
  a.push(r + 1, r + 2, r + 3, r + 4, r + 5);      // outgrows the cell
  const b = lit(r);
  b[100] = "far";                                   // slow array
  const c = lit(r);
  delete c[1];                                      // a hole
  c.length = 2;                                     // shrinks
  const d = lit(r).concat(lit(r + 1)).slice(1, 6);
  const e = [5, 3, 9, 1, r % 7].sort((x, y) => x - y);
  const f = lit(r); f.splice(1, 2, "x", "y", "z");
  const g = lit(r); g.unshift("u"); g.shift(); g.reverse();
  const m = /(\d+)-(\d+)/.exec("a " + r + "-" + (r * 2) + " b");
  m.push("extra"); m.foo = 1;
  const h = Array.from(lit(r), (x) => typeof x);
  const k = [...lit(r), ...lit(r)];
  const fr = Object.freeze(lit(r));
  if (r % 50 === 0) out.push(JSON.stringify([a, b.length, b[100], c, d, e, f, g, m, h, k.length, Object.isFrozen(fr), fr[2].v]));
  keep.push(a, f, m);
  if (keep.length > 200) keep = keep.slice(100);
  for (let i = 0; i < 300; i++) [i, { i }];
}
let sum = 0;
for (const x of keep) sum += x.length;
out.push("sum " + sum);
console.log(out.join("\n"));
