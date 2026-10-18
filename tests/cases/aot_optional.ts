// @native errorOf scoreOr depth something total
// Optional fields (`x?: T`, `T | undefined`: absent, then `undefined`;
// set to `undefined` or deleted, absent again), fields that hold `null`
// (`T | null`), and `aot type`s that also hold `null` / `undefined`.
type int = number;
aot type Node = { id: int; label?: string; score?: number; parent: Node | null; next?: Node };
aot type F = { something?: number } | null | undefined;
aot type User = { name: string; age: int | undefined; email: string | null; tags?: { t: string }[] };

function errorOf(f: () => void): string {
  try { f(); return "ok"; } catch (e) { return (e as Error).constructor.name + ": " + (e as Error).message; }
}
function scoreOr(n: Node, d: number): number {
  const s = n.score;
  return s === undefined ? d : s * 2;
}
function depth(n: Node): int {
  let d: int = 0;
  let p = n.parent;
  while (p !== null) { d = (d + 1) | 0; p = p.parent; }
  return d;
}
function something(f: F): string {
  if (f === null) return "null";
  if (f === undefined) return "undefined";
  return String(f.something);
}
const root: Node = { id: 1, parent: null };
const child: Node = { id: 2, label: "c", score: 1.5, parent: root };
console.log(JSON.stringify(root), JSON.stringify(child), Object.keys(root).join(","), "score" in root);
console.log(scoreOr(root, -1), scoreOr(child, -1), depth(child), depth(root));
root.score = 4;
console.log(scoreOr(root, -1), JSON.stringify(root), "score" in root);
root.score = undefined;
console.log(scoreOr(root, -1), JSON.stringify(root), "score" in root);
child.label = "relabeled";
console.log(delete child.label, JSON.stringify(child), errorOf(() => delete (child as any).id));
console.log(errorOf(() => ((root as any).parent = undefined)), errorOf(() => ((root as any).score = "x")));
root.next = child;
console.log(root.next.id, errorOf(() => ((root as any).next = { id: 3, parent: null })));
console.log(something(null), something(undefined), something({}), something({ something: 3 }));
const u: User = { name: "a", age: undefined, email: null };
const v: User = { name: "b", age: 30, email: "b@x", tags: [{ t: "x" }] };
console.log(JSON.stringify(u), JSON.stringify(v), Object.keys(u).join(","));
console.log(errorOf(() => ((u as any).email = undefined)), errorOf(() => ((u as any).age = null)));
function total(us: User[]): int {
  let t: int = 0;
  for (let i: int = 0; i < us.length; i++) {
    const a = us[i].age;
    if (a !== undefined) t = (t + a) | 0;
  }
  return t;
}
const us: User[] = [u, v, { name: "c", age: 12, email: null }];
console.log(total(us), JSON.stringify(Object.getOwnPropertyDescriptor(v, "tags")), JSON.stringify(Object.getOwnPropertyDescriptor(v, "name")));
const w: User = { name: "w", age: 1, email: null };
w.tags = [{ t: "added" }];
console.log(JSON.stringify(w), errorOf(() => ((w as any).tags = [{ t: 1 }])), errorOf(() => ((w as any).nope = 1)));
