// @native pairs pairsAfter innerOnly skipSelf withBreak outerJump countPairs
// Numeric regions over a loop nest that reads objects from one array in
// both loops (`const bi = bs[i]; for (j) { const bj = bs[j]; ... }`): the
// outer iteration's element is checked once per outer iteration, the inner
// one in each inner iteration. Where an inner element is not what the region
// needs (another shape, a string field, a hole, the outer element itself),
// the original code goes on from that inner iteration: the rest of the
// inner loop, the rest of the outer iteration, the outer loop from its next
// iteration, with the same results. A jump to the outer loop from the inner
// one keeps the nest from being one region.

type P = { x: number; y: number; vx: number; vy: number; m: number };

function mk(n: number, ints: boolean): P[] {
  const out: P[] = [];
  for (let i = 0; i < n; i++) {
    const f = ints ? 1 : 1.5;
    out.push({ x: i * f, y: (i % 4) * f - 2, vx: 0, vy: ints ? 1 : 0.25, m: 1 + (i % 3) * f });
  }
  return out;
}

function pairs(bs: P[], dt: number): number {
  const n = bs.length;
  for (let i = 0; i < n; i++) {
    const bi = bs[i];
    for (let j = i + 1; j < n; j++) {
      const bj = bs[j];
      const dx = bi.x - bj.x;
      const dy = bi.y - bj.y;
      const d2 = dx * dx + dy * dy + 0.01;
      const mag = dt / (d2 * Math.sqrt(d2));
      bi.vx -= dx * bj.m * mag;
      bi.vy -= dy * bj.m * mag;
      bj.vx += dx * bi.m * mag;
      bj.vy += dy * bi.m * mag;
    }
  }
  let s = 0;
  for (let i = 0; i < n; i++) s += bs[i].vx * 3 + bs[i].vy;
  return s;
}

// Work after the inner loop, in the outer iteration.
function pairsAfter(bs: P[]): number {
  let total = 0;
  for (let i = 0; i < bs.length; i++) {
    const bi = bs[i];
    let near = 0;
    for (let j = 0; j < bs.length; j++) {
      const bj = bs[j];
      const d = Math.abs(bi.x - bj.x) + Math.abs(bi.y - bj.y);
      if (d < 3) near += bj.m;
    }
    bi.vx = near;
    total += near * (i + 1);
  }
  return total;
}

// Only the inner loop reads elements.
function innerOnly(bs: P[], k: number): number {
  let acc = 0;
  for (let r = 0; r < k; r++) {
    for (let j = 0; j < bs.length; j++) {
      const bj = bs[j];
      acc += bj.x * r - bj.y;
    }
    acc = acc * 0.5;
  }
  return acc;
}

// Inner iterations over every element, the outer one among them.
function skipSelf(bs: P[]): number {
  let s = 0;
  for (let i = 0; i < bs.length; i++) {
    const bi = bs[i];
    for (let j = 0; j < bs.length; j++) {
      if (j === i) continue;
      const bj = bs[j];
      bi.vy += (bj.x - bi.x) * 0.125;
      s += bj.m;
    }
  }
  return s + bs[0].vy;
}

function withBreak(bs: P[], limit: number): number {
  let s = 0;
  for (let i = 0; i < bs.length; i++) {
    const bi = bs[i];
    for (let j = i + 1; j < bs.length; j++) {
      const bj = bs[j];
      if (bi.x + bj.x > limit) break;
      s += bi.x * bj.y;
    }
  }
  return s;
}

function outerJump(bs: P[]): number {
  let s = 0;
  outer: for (let i = 0; i < bs.length; i++) {
    const bi = bs[i];
    for (let j = 0; j < bs.length; j++) {
      const bj = bs[j];
      if (bj.x > bi.x + 4) continue outer;
      s += bj.y - bi.y;
    }
    s += 1;
  }
  return s;
}

function countPairs(bs: P[]): number {
  let c = 0;
  for (let i = 0; i < bs.length; i++) {
    const bi = bs[i];
    for (let j = 0; j < bs.length; j++) {
      const bj = bs[j];
      if (bi.m < bj.m) c++;
    }
  }
  return c;
}

function show(label: string, v: number): void {
  console.log(label, Math.round(v * 1e6) / 1e6);
}

for (const ints of [false, true]) {
  const a = mk(12, ints);
  show("pairs", pairs(a, 0.01));
  show("after", pairsAfter(mk(10, ints)));
  show("inner", innerOnly(mk(9, ints), 5));
  show("skip", skipSelf(mk(8, ints)));
  show("break", withBreak(mk(10, ints), 9));
  show("jump", outerJump(mk(10, ints)));
  show("count", countPairs(mk(11, ints)));
}

// Elements that are not what the regions need, at various positions.
const odd: any[] = mk(10, false);
odd[6] = { x: 1, y: 2, vx: 0, vy: 0, m: 1, extra: true }; // another shape
show("odd pairs", pairs(odd, 0.02));
show("odd after", pairsAfter(odd));
const str: any[] = mk(9, false);
str[4].m = "2"; // a string field
show("str pairs", pairs(str, 0.01));
show("str count", countPairs(str));
const holey: P[] = mk(8, false);
holey.length = 10; // holes past the elements
try {
  show("holey", pairs(holey, 0.01));
} catch (e) {
  console.log("holey threw", (e as Error).constructor.name);
}
const twice = mk(7, false);
twice[5] = twice[2]; // the same object twice
show("twice pairs", pairs(twice, 0.01));
show("twice skip", skipSelf(twice));
show("twice inner", innerOnly(twice, 3));
const shared = mk(6, true);
shared[0] = shared[5];
show("shared after", pairsAfter(shared));
