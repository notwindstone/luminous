// JSON.stringify of plain objects and of `aot type` objects (written from
// their fields), and of arrays of 100 of them: ns per call.
//   luminous run bench/micro/json.ts
// For bun or node, without `aot` (`aot type` is a plain `type` to them):
//   sed 's/^aot type/type/' bench/micro/json.ts > /tmp/json.ts && bun /tmp/json.ts
aot type Point = { x: number; y: number };
aot type User = { id: int; name: string; email: string; age: int; score: number; active: boolean; home: Point };

type PPoint = { x: number; y: number };
type PUser = { id: number; name: string; email: string; age: number; score: number; active: boolean; home: PPoint };

function typed(i: int): User {
  return { id: i, name: "user" + i, email: "user" + i + "@example.com", age: 20 + (i % 50), score: i * 1.5, active: i % 2 === 0, home: { x: i, y: i / 2 } };
}

function plain(i: number): PUser {
  return { id: i, name: "user" + i, email: "user" + i + "@example.com", age: 20 + (i % 50), score: i * 1.5, active: i % 2 === 0, home: { x: i, y: i / 2 } };
}

function bench(label: string, n: number, f: () => number): void {
  let s = 0;
  for (let i = 0; i < n / 10; i++) s += f();
  const t0 = performance.now();
  for (let i = 0; i < n; i++) s += f();
  const dt = performance.now() - t0;
  console.log(label.padEnd(16) + ((dt * 1e6) / n).toFixed(0).padStart(8) + " ns  " + s);
}

const tu = typed(12345);
const pu = plain(12345);
const ta: User[] = [];
const pa: PUser[] = [];
for (let i = 0; i < 100; i++) {
  ta.push(typed(i));
  pa.push(plain(i));
}
if (JSON.stringify(tu) !== JSON.stringify(pu) || JSON.stringify(ta) !== JSON.stringify(pa)) throw new Error("differ");

bench("plain object", 2000000, () => JSON.stringify(pu).length);
bench("typed object", 2000000, () => JSON.stringify(tu).length);
bench("plain array 100", 20000, () => JSON.stringify(pa).length);
bench("typed array 100", 20000, () => JSON.stringify(ta).length);
