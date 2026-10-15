// Async functions, awaits and promises: ns per iteration of each loop.
//   luminous run bench/micro/async.ts
//   bun bench/micro/async.ts
async function plain(x: number): Promise<number> {
  return x + 1;
}

async function inner(x: number): Promise<number> {
  return x * 2;
}

async function outer(x: number): Promise<number> {
  return (await inner(x)) + 1;
}

// As a framework's middleware chain: each awaits the next.
type Next = () => Promise<void>;
type Ctx = { n: number };
const middleware: Array<(c: Ctx, next: Next) => Promise<void>> = [
  async (c, next) => {
    c.n++;
    await next();
  },
  async (c, next) => {
    await next();
    c.n++;
  },
  async (c, next) => {
    c.n += 2;
    await next();
  },
];

function compose(c: Ctx): Promise<void> {
  const dispatch = async (i: number): Promise<void> => {
    if (i === middleware.length) return;
    await middleware[i](c, () => dispatch(i + 1));
  };
  return dispatch(0);
}

async function bench(label: string, n: number, f: (i: number) => Promise<number>): Promise<void> {
  let s = 0;
  for (let i = 0; i < n / 10; i++) s += await f(i);
  const t0 = performance.now();
  for (let i = 0; i < n; i++) s += await f(i);
  const dt = performance.now() - t0;
  console.log(label.padEnd(22) + ((dt * 1e6) / n).toFixed(0).padStart(6) + " ns  " + s);
}

async function main(): Promise<void> {
  await bench("await primitive", 2000000, async (i) => i);
  await bench("await async call", 2000000, (i) => plain(i));
  await bench("await nested async", 1000000, (i) => outer(i));
  await bench("await Promise.resolve", 2000000, (i) => Promise.resolve(i));
  await bench("await new Promise", 1000000, (i) => new Promise<number>((resolve) => resolve(i)));
  await bench("await later resolved", 1000000, (i) => new Promise<number>((resolve) => queueMicrotask(() => resolve(i))));
  await bench("Promise.all of 4", 500000, async (i) => {
    const r = await Promise.all([plain(i), plain(i + 1), plain(i + 2), plain(i + 3)]);
    return r[0] + r[3];
  });
  await bench("then chain of 3", 1000000, (i) => plain(i).then((x) => x + 1).then((x) => x * 2));
  await bench("middleware of 3", 500000, async (i) => {
    const c = { n: i };
    await compose(c);
    return c.n;
  });
}

main();
