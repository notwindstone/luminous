// Per-request response records that must be real objects: helpers too
// large to inline write their fields, and each record is serialized
// (JSON.stringify) and its keys counted. None outlives its request: the
// sync handler, then an async one whose helpers write the record across
// awaits.
type Res = { status: number; bytes: number; route: string; cached: boolean; retries: number };

const paths = ["/", "/users", "/users/42", "/static/app.js"];

function classify(r: Res, path: string): void {
  let n = 0;
  for (let i = 0; i < path.length; i++) n += path.charCodeAt(i) & 7;
  if (n > 40) {
    r.route = "long";
    r.retries = 2;
  } else if (n > 10) {
    r.route = "mid";
    r.retries = 1;
  } else {
    r.route = "short";
  }
  r.bytes += n;
}

function finish(r: Res, code: number): void {
  let b = 0;
  for (let i = 0; i < 3; i++) b += (i * code) & 3;
  if (code % 13 === 0) {
    r.status = 404;
    r.cached = false;
  } else if (code % 7 === 0) {
    r.status = 304;
    r.cached = true;
  } else {
    r.status = 200;
  }
  r.bytes += b;
}

function handle(i: number): number {
  const r = { status: 0, bytes: 0, route: "", cached: (i & 1) === 0, retries: 0 };
  classify(r, paths[i & 3]);
  finish(r, i);
  const body = JSON.stringify(r);
  return r.status + r.bytes + body.length + Object.keys(r).length;
}

async function load(r: Res, k: number): Promise<void> {
  await null;
  for (let i = 0; i < 2; i++) r.bytes += k + i;
  if (k > 5) r.retries += 1;
}

async function handleAsync(i: number): Promise<number> {
  const r = { status: 0, bytes: 0, route: "", cached: false, retries: 0 };
  await load(r, i & 7);
  finish(r, i);
  await load(r, 1);
  return r.status + r.bytes + r.retries;
}

async function main(): Promise<void> {
  const start = Date.now();
  let sync = 0;
  for (let i = 0; i < 2_000_000; i++) sync += handle(i);
  const mid = Date.now();
  let asyncTotal = 0;
  for (let i = 0; i < 500_000; i++) asyncTotal += await handleAsync(i);
  const end = Date.now();
  console.log(`sync: ${sync}`);
  console.log(`async: ${asyncTotal}`);
  console.log(`elapsed sync: ${mid - start} ms`);
  console.log(`elapsed async: ${end - mid} ms`);
  console.log(`elapsed: ${end - start} ms`);
}

main();
