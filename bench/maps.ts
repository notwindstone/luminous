// Maps and sets: word frequencies (string keys), a graph's adjacency
// (integer keys), deduplication, memoization.
function words(n: number): string[] {
  const syll = ["ka", "lo", "mi", "ne", "su", "ta", "ri", "po", "ve", "zu"];
  const out: string[] = [];
  let x = 12345;
  for (let i = 0; i < n; i++) {
    x = (x * 1103515245 + 12345) & 0x7fffffff;
    const len = 1 + (x % 3);
    let w = "";
    for (let k = 0; k < len; k++) w += syll[(x >> (k * 4)) % 10];
    out.push(w);
  }
  return out;
}

function frequencies(ws: string[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const w of ws) m.set(w, (m.get(w) ?? 0) + 1);
  return m;
}

function adjacency(n: number, edges: number): Map<number, number[]> {
  const g = new Map<number, number[]>();
  let x = 7;
  for (let i = 0; i < edges; i++) {
    x = (x * 1103515245 + 12345) & 0x7fffffff;
    const a = x % n;
    const b = (x >> 8) % n;
    let l = g.get(a);
    if (l === undefined) {
      l = [];
      g.set(a, l);
    }
    l.push(b);
  }
  return g;
}

function reach(g: Map<number, number[]>, from: number): number {
  const seen = new Set<number>();
  const stack = [from];
  while (stack.length > 0) {
    const v = stack.pop()!;
    if (seen.has(v)) continue;
    seen.add(v);
    const l = g.get(v);
    if (l) for (const w of l) if (!seen.has(w)) stack.push(w);
  }
  return seen.size;
}

const memo = new Map<string, number>();
function cost(a: number, b: number): number {
  const key = a + ":" + b;
  const c = memo.get(key);
  if (c !== undefined) return c;
  const v = (a * 31 + b * 17) % 101;
  memo.set(key, v);
  return v;
}

const start = Date.now();
let total = 0;
for (let r = 0; r < 5; r++) {
  const ws = words(200_000);
  const f = frequencies(ws);
  let top = 0;
  for (const [, c] of f) if (c > top) top = c;
  const uniq = new Set(ws);
  const g = adjacency(50_000, 200_000);
  let s = 0;
  for (let i = 0; i < 100_000; i++) s += cost(i % 300, (i * 7) % 300);
  total += f.size + top + uniq.size + reach(g, 0) + s;
}
console.log(`total: ${total}`);
console.log(`elapsed: ${Date.now() - start} ms`);
