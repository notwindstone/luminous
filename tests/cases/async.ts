// @native sum sequence safe looped makeWorker fails retNumber typed run nested lazy pollUntil
const log: string[] = [];
const delay = (v: unknown) => Promise.resolve(v);

aot async function sum(a: number, b: number): Promise<number> {
  log.push("sum start");
  const x = await delay(a);
  log.push("sum after first await");
  const y = await b;
  return (x as number) + (y as number);
}
aot async function sequence(): Promise<string> {
  let out = "";
  out = out + (await sum(1, 2));
  return out + "!";
}
aot async function safe(): Promise<string> {
  try {
    await Promise.reject(new TypeError("nope"));
    return "unreachable";
  } catch (e) {
    return "caught " + (e as Error).message;
  }
}
aot async function looped(n: number): Promise<number> {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const v = await delay(i);
    total += v as number;
    log.push("loop " + i);
  }
  return total;
}
aot function makeWorker(name: string) {
  let jobs = 0;
  return async (job: string) => {
    jobs++;
    await null;
    return name + ":" + job + "#" + jobs;
  };
}
aot async function fails(): Promise<void> {
  await 1;
  throw new RangeError("async failure");
}
aot async function retNumber(x: number): Promise<number> {
  return x * 2;
}
aot async function typed(x: number): Promise<string> {
  await 0;
  return typeof x;
}
// `await` nested in expressions: everything evaluated before an `await`
// in JS order is computed before it suspends.
let counter = 0;
const bump = async (): Promise<number> => ++counter;
aot async function nested(): Promise<string> {
  const parts: string[] = [];
  let x = 1;
  const sum2 = x + (await (async () => { x = 100; return 2; })());
  parts.push("sum " + sum2 + " " + x);
  parts.push(`tpl ${counter} ${await bump()} ${counter}`);
  const obj: Record<string, unknown> = { a: counter, b: await bump(), c: counter };
  parts.push(JSON.stringify(obj));
  const arr = [counter, await bump(), counter, await delay("d")];
  parts.push(JSON.stringify(arr));
  const o: Record<string, number> = { k: 1 };
  o.k += (await bump()) as number;
  o[(await delay("j")) as string] = (await bump()) as number;
  parts.push(JSON.stringify(o));
  let acc = 10;
  acc -= (await delay(3)) as number;
  parts.push("acc " + acc);
  parts.push(Math.max(1, (await delay(7)) as number).toString());
  parts.push([1, 2].concat([(await delay(3)) as number]).join(","));
  let n = 0;
  const v = n++ + ((await delay(n)) as number) + n;
  parts.push("order " + v);
  return parts.join("; ");
}
aot async function lazy(flag: boolean): Promise<string> {
  let hits = 0;
  const hit = async (v: unknown) => { hits++; return v; };
  const a = flag && (await hit("and"));
  const b = flag || (await hit("or"));
  const c = (flag ? null : undefined) ?? (await hit("nullish"));
  const d = flag ? await hit("then") : await hit("else");
  if ((await hit(flag)) === true) hits += 100;
  return [a, b, c, d, hits].join(",");
}
aot async function pollUntil(limit: number): Promise<number> {
  let polls = 0;
  while (((await delay(polls)) as number) < limit) polls++;
  for (let i = 0; i < ((await delay(2)) as number); i++) polls += 10;
  return polls;
}
class Service {
  base = 10;
  async run(x: number): Promise<number> {
    const v = await delay(x);
    return this.base + (v as number);
  }
}

// An interpreted async function interleaved with native ones: the order of
// all these log entries depends on exact microtask scheduling.
async function interp() {
  log.push("interp start");
  await null;
  log.push("interp after await");
}

async function main() {
  const p1 = sum(2, 3);
  const p2 = interp();
  Promise.resolve().then(() => log.push("then 1")).then(() => log.push("then 2"));
  log.push("sync end");
  console.log(await p1, await p2, await sequence(), await safe(), await looped(3));
  const w = makeWorker("w");
  console.log(await Promise.all([w("a"), w("b")]), await retNumber(21));
  try {
    await fails();
  } catch (e) {
    console.log("rejected:", (e as Error).name, (e as Error).message);
  }
  console.log(await typed(1), await (typed as (x: unknown) => Promise<string>)("s"));
  console.log(await nested());
  console.log(await lazy(true), await lazy(false), await pollUntil(3));
  console.log(await new Service().run(5), Object.prototype.toString.call(sum(0, 0)));
  console.log(log.join(" | "));
}
main();
