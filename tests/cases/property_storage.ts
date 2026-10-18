// Objects whose properties outgrow their cell, then shrink, are deleted,
// frozen, collected; classes with data, functions, arrays with names.
const out = [];
let keep = [];
for (let r = 0; r < 400; r++) {
  const o = {};
  for (let i = 0; i < 12; i++) o["p" + i] = { i, r };
  delete o.p3; delete o.p7;
  for (let i = 12; i < 70; i++) o["q" + (i % 40)] = i;
  const f = function () {}; for (let i = 0; i < 9; i++) f["s" + i] = [i];
  const a = [1, 2, 3]; for (let i = 0; i < 9; i++) a["n" + i] = "v" + i;
  const m = new Map([[1, 2]]); for (let i = 0; i < 9; i++) m["k" + i] = i;
  const big = {}; for (let i = 0; i < 100; i++) big["b" + i] = i;
  for (let i = 0; i < 60; i++) delete big["b" + i];
  big.after = "x";
  const fr = Object.freeze({ ...o });
  if (r % 80 === 0) out.push(JSON.stringify([o, Object.keys(f), a, Object.keys(m), Object.keys(big).length, big.b99, Object.isFrozen(fr)]));
  keep.push(o, f, a, big);
  if (keep.length > 300) keep = keep.slice(150);
  for (let i = 0; i < 400; i++) ({ i, j: [i] });
}
let n = 0;
for (const k of keep) n += Object.keys(k).length;
out.push("n " + n);
console.log(out.join("\n"));
