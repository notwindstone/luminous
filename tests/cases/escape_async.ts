// @native nested run later mutating constant step task wrap resolved rejecter rejected timer
// Escape summaries across `await`: an async function that only reads the
// object it is given is called through a clone that takes the fields,
// where the caller awaits it directly or never writes the object again.
// The order of promise reactions is that of JS: the clone settles its
// promise where the function would.
import { setTimeout } from "qjs:os";

const external = { n: 5 };
const log: string[] = [];

function tick(): Promise<void> {
  return Promise.resolve();
}

async function nested(p: any): Promise<number> {
  await tick();
  const x = p.a + p.b;
  await tick();
  return x + p.ref.n;
}

async function run(k: number): Promise<number> {
  const o = { a: k, b: k * 2, ref: external };
  const v = await nested(o);
  return v + o.a;
}

async function later(p: any): Promise<number> {
  await tick();
  return p.a;
}

// Not awaited, and written after the call: the callee sees the write.
async function mutating(): Promise<string> {
  const o = { a: 1 };
  const pr = later(o);
  o.a = 2;
  const v = await pr;
  return "mutating " + v + " " + o.a;
}

// Not awaited, never written: the fields can be copied.
async function constant(): Promise<number> {
  const o = { a: 3 };
  const pr = later(o);
  await tick();
  return (await pr) + o.a;
}

async function step(p: any, tag: string): Promise<number> {
  log.push(tag + " start " + p.a);
  await tick();
  log.push(tag + " mid " + p.b);
  await tick();
  return p.a + p.b;
}

async function task(tag: string, k: number): Promise<number> {
  const o = { a: k, b: 2 * k };
  const r = await step(o, tag);
  log.push(tag + " done " + r);
  return r + o.a;
}

// `Promise.resolve(o)` keeps the object.
async function wrap(p: any): Promise<any> {
  await tick();
  return Promise.resolve(p);
}

async function resolved(): Promise<string> {
  const o = { a: 1 };
  const r = await wrap(o);
  return "resolved " + (r === o);
}

async function rejecter(p: any): Promise<number> {
  await tick();
  if (p.a > 0) throw p;
  return 0;
}

async function rejected(): Promise<string> {
  const o = { a: 1 };
  try {
    await rejecter(o);
    return "no";
  } catch (e) {
    return "rejected " + (e === o);
  }
}

// Captured by a closure that a timer runs later: it escapes.
function timer(): Promise<string> {
  const o = { a: 1 };
  const p = new Promise<string>((res) => setTimeout(() => res("timer " + o.a), 0));
  o.a = 2;
  return p;
}

async function main(): Promise<void> {
  console.log(await run(3), await mutating(), await constant());
  const both = await Promise.all([task("x", 1), task("y", 2)]);
  log.push("all " + both.join(","));
  console.log(log.join("; "));
  console.log(await resolved(), await rejected(), await timer());
}

main();
