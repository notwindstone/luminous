// Live data a server keeps: records with string, number and nested fields,
// arrays of numbers, a Map index (peak memory is what it measures).
type User = { id: number; name: string; email: string; age: number; tags: string[]; score: number };
function main(): void {
  const start = Date.now();
  const users: User[] = [];
  for (let i = 0; i < 300000; i++) {
    users.push({ id: i, name: "user" + i, email: "u" + i + "@example.com", age: i % 90, tags: ["a", "b"], score: i * 0.5 });
  }
  const index = new Map<string, User>();
  for (const u of users) index.set(u.email, u);
  const grid: number[][] = [];
  for (let i = 0; i < 1000; i++) {
    const row: number[] = [];
    for (let j = 0; j < 500; j++) row.push(i * j);
    grid.push(row);
  }
  let s = 0;
  for (const u of users) s += u.score + u.age;
  for (const row of grid) s += row[499];
  console.log(index.size, users.length, s);
  console.log(`elapsed: ${Date.now() - start} ms`);
}
main();
