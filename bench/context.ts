// A per-request context object, made for each request and handed through a
// chain of helpers that are too large to inline (each has a loop): the
// sync chain, then the same chain with async helpers awaited in turn.
type Ctx = { id: number; method: number; path: string; limit: number; retries: number; verbose: boolean };

const paths = ["/", "/users", "/users/42", "/static/app.js"];

function authorize(ctx: Ctx): number {
  let h = ctx.id;
  for (let i = 0; i < ctx.retries; i++) h = (h * 31 + ctx.method) | 0;
  return h & 1023;
}

function route(ctx: Ctx, key: number): number {
  let n = key;
  const path = ctx.path;
  for (let i = 0; i < path.length; i++) n += path.charCodeAt(i);
  return n % ctx.limit;
}

function render(ctx: Ctx, slot: number): number {
  let s = slot;
  for (let i = 0; i < 4; i++) s += ctx.verbose ? i * 2 : i;
  return s + (ctx.id % 7);
}

function handle(ctx: Ctx): number {
  const key = authorize(ctx);
  const slot = route(ctx, key);
  return render(ctx, slot) + audit(ctx, slot);
}

function audit(ctx: Ctx, slot: number): number {
  let a = 0;
  for (let i = 0; i < 2; i++) a += (ctx.method + slot + i) & 3;
  return a;
}

function serve(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const ctx = { id: i, method: i & 3, path: paths[i & 3], limit: 97 + (i & 7), retries: 3, verbose: (i & 15) === 0 };
    total += handle(ctx);
  }
  return total;
}

async function authorizeAsync(ctx: Ctx): Promise<number> {
  let h = ctx.id;
  for (let i = 0; i < ctx.retries; i++) h = (h * 31 + ctx.method) | 0;
  return h & 1023;
}

async function renderAsync(ctx: Ctx, slot: number): Promise<number> {
  let s = slot;
  for (let i = 0; i < 4; i++) s += ctx.verbose ? i * 2 : i;
  return s + (ctx.id % 7);
}

async function handleAsync(ctx: Ctx): Promise<number> {
  const key = await authorizeAsync(ctx);
  const slot = route(ctx, key);
  const r = await renderAsync(ctx, slot);
  return r + audit(ctx, slot);
}

async function serveAsync(n: number): Promise<number> {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const ctx = { id: i, method: i & 3, path: paths[i & 3], limit: 97 + (i & 7), retries: 3, verbose: (i & 15) === 0 };
    total += await handleAsync(ctx);
  }
  return total;
}

async function main(): Promise<void> {
  const start = Date.now();
  let sync = 0;
  for (let round = 0; round < 10; round++) sync += serve(1_000_000);
  const mid = Date.now();
  const asyncTotal = await serveAsync(1_000_000);
  const end = Date.now();
  console.log(`sync: ${sync}`);
  console.log(`async: ${asyncTotal}`);
  console.log(`elapsed sync: ${mid - start} ms`);
  console.log(`elapsed async: ${end - mid} ms`);
  console.log(`elapsed: ${end - start} ms`);
}

main();
