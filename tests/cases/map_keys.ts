// @native ints floats objects strings mixed
// Maps and Sets of many keys of one kind (small integers, doubles, objects,
// strings) and of mixed kinds, with deletions while the table grows and
// shrinks: lookups, sizes and the insertion order of iteration are those
// of the language whatever the hash of the keys; `0` and `-0` are one key,
// `NaN` is one key, `1` and `1.0` are one key.

function ints(n: number): string {
  const m = new Map<number, number>();
  for (let i = 0; i < n; i++) m.set(i * 4, i);
  for (let i = 0; i < n; i += 3) m.delete(i * 4);
  let hit = 0;
  let sum = 0;
  for (let i = 0; i < n * 4; i++) {
    const v = m.get(i);
    if (v !== undefined) {
      hit++;
      sum += v;
    }
  }
  const s = new Set<number>();
  for (let i = -n; i < n; i++) s.add(i % 97);
  let first = "";
  for (const k of m.keys()) {
    first += k + ",";
    if (first.length > 20) break;
  }
  return [m.size, hit, sum, s.size, s.has(-96), s.has(97), first].join(" ");
}

function floats(n: number): string {
  const m = new Map<number, string>();
  for (let i = 0; i < n; i++) m.set(i + 0.5, "h" + i);
  m.set(-0, "zero");
  m.set(NaN, "nan");
  m.set(0 / 0, "nan2");
  m.set(1, "one");
  let hit = 0;
  for (let i = 0; i < n; i++) if (m.get(i + 0.5) === "h" + i) hit++;
  for (let i = 0; i < n; i += 2) m.delete(i + 0.5);
  return [m.size, hit, m.get(0), m.get(-0), m.get(NaN), m.get(1.0), m.get(2.5), m.get(3.5), m.has(1e300)].join(" ");
}

class Node {
  constructor(public id: number) {}
}

function objects(n: number): string {
  const nodes: Node[] = [];
  for (let i = 0; i < n; i++) nodes.push(new Node(i));
  const m = new Map<Node, number>();
  const seen = new Set<object>();
  for (const x of nodes) {
    m.set(x, x.id * 2);
    seen.add(x);
  }
  for (let i = 0; i < n; i += 5) {
    m.delete(nodes[i]);
    seen.delete(nodes[i]);
  }
  let sum = 0;
  for (const x of nodes) sum += m.get(x) ?? -1;
  let order = 0;
  let last = -1;
  for (const [k, v] of m) {
    if (k.id > last) order++;
    last = k.id;
    if (v !== k.id * 2) order = -1000;
  }
  return [m.size, seen.size, sum, order, m.has(new Node(1)), seen.has(nodes[1])].join(" ");
}

function strings(n: number): string {
  const m = new Map<string, number>();
  for (let i = 0; i < n; i++) {
    const k = "k" + (i % 50) + "_" + ((i * 7) % 13);
    m.set(k, (m.get(k) ?? 0) + 1);
  }
  const s = new Set<string>();
  for (const k of m.keys()) s.add(k.slice(0, 3));
  let total = 0;
  for (const v of m.values()) total += v;
  return [m.size, total, s.size, m.get("k3_8"), m.get("k3_9"), [...s].slice(0, 6).join("/")].join(" ");
}

function mixed(): string {
  const o = {};
  const m = new Map<unknown, number>();
  const keys: unknown[] = [1, "1", 1.5, true, null, undefined, o, -0, NaN, Symbol.iterator, "", 0];
  keys.forEach((k, i) => m.set(k, i));
  const out = keys.map((k) => String(m.get(k)));
  out.push(String(m.get(0)), String(m.get({})), String(m.get(1.0)), String(m.size));
  return out.join(",");
}

console.log(ints(5000));
console.log(floats(3000));
console.log(objects(4000));
console.log(strings(3000));
console.log(mixed());
