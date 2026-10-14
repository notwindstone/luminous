// Cost of crossing from interpreted code into native code, per call.
// Build twice and compare:
//   luminous run bench/micro/crossing.ts               (id/lerp native)
//   luminous run --no-native bench/micro/crossing.ts   (everything interpreted)
// The loops themselves stay interpreted ("use no aot"), so "call - base"
// is what one call costs, including the native work.
function id(x: number): number {
  return x;
}
function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * t;
}

function idCall(n) {
  "use no aot";
  let acc = 0;
  for (let i = 0; i < n; i++) acc += id(i);
  return acc;
}
function idBase(n) {
  "use no aot";
  let acc = 0;
  for (let i = 0; i < n; i++) acc += i;
  return acc;
}
function lerpCall(n) {
  "use no aot";
  let acc = 0;
  for (let i = 0; i < n; i++) acc += lerp(i, i + 1, 0.25);
  return acc;
}
function lerpBase(n) {
  "use no aot";
  let acc = 0;
  for (let i = 0; i < n; i++) acc += i + (i + 1 - i) * 0.25;
  return acc;
}

function nsPerIter(f, n) {
  "use no aot";
  f(1000);
  let best = Infinity;
  for (let round = 0; round < 3; round++) {
    const t0 = performance.now();
    f(n);
    best = Math.min(best, ((performance.now() - t0) * 1e6) / n);
  }
  return best;
}

const N = 10_000_000;
// Untimed warmup, so the first measurement does not include CPU clock ramp-up.
for (const f of [idCall, idBase, lerpCall, lerpBase]) f(N);
for (const [name, call, base] of [["id", idCall, idBase], ["lerp", lerpCall, lerpBase]] as const) {
  const c = nsPerIter(call, N);
  const b = nsPerIter(base, N);
  console.log(`${name.padEnd(5)} call ${c.toFixed(1)} ns  base ${b.toFixed(1)} ns  => ${(c - b).toFixed(1)} ns per call`);
}
