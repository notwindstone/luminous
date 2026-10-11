// Records processing: sorting with a comparator, map / filter / reduce,
// grouping into an object, string keys.
type Employee = { id: number; name: string; dept: string; salary: number; age: number };

function makeEmployees(n: number): Employee[] {
  const depts = ["eng", "sales", "ops", "hr", "legal"];
  const out: Employee[] = [];
  let seed = 42;
  for (let i = 0; i < n; i++) {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    out.push({ id: i, name: "emp" + i, dept: depts[seed % 5], salary: 30000 + (seed % 90000), age: 20 + (seed % 45) });
  }
  return out;
}

function summarize(emps: Employee[]): number {
  const senior = emps.filter((e) => e.age > 40);
  const raises = senior.map((e) => ({ id: e.id, salary: e.salary * 1.05 }));
  const total = raises.reduce((acc, r) => acc + r.salary, 0);
  const byDept: Record<string, number> = {};
  for (const e of emps) byDept[e.dept] = (byDept[e.dept] ?? 0) + e.salary;
  const sorted = emps.slice().sort((a, b) => b.salary - a.salary || a.id - b.id);
  return total + byDept["eng"] + sorted[0].salary + sorted[sorted.length - 1].id;
}

const start = Date.now();
const emps = makeEmployees(100000);
let acc = 0;
for (let k = 0; k < 10; k++) acc += summarize(emps);
console.log(`result: ${acc.toFixed(2)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
