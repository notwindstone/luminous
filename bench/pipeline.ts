// An async pipeline: each job's context object is handed from stage to
// stage across awaits (each stage awaits too).
type Job = { id: number; size: number; weight: number; tag: string; priority: number };

const tags = ["red", "green", "blue", "gray"];

function tick(): Promise<void> {
  return Promise.resolve();
}

async function parse(job: Job): Promise<number> {
  await tick();
  let n = 0;
  for (let i = 0; i < job.tag.length; i++) n += job.tag.charCodeAt(i);
  return n + job.size;
}

async function transform(job: Job, x: number): Promise<number> {
  await tick();
  let v = x;
  for (let i = 0; i < 3; i++) v = (v * job.weight + job.priority) % 1000;
  return v;
}

async function store(job: Job, v: number): Promise<number> {
  await tick();
  return (v + job.id) % 997;
}

async function run(id: number): Promise<number> {
  const job = { id, size: id % 100, weight: 1.5, tag: tags[id & 3], priority: id & 7 };
  const a = await parse(job);
  const b = await transform(job, a);
  return store(job, b);
}

async function main(): Promise<void> {
  const start = Date.now();
  let total = 0;
  for (let i = 0; i < 600_000; i++) total += await run(i);
  console.log(`total: ${total.toFixed(3)}`);
  console.log(`elapsed: ${Date.now() - start} ms`);
}

main();
