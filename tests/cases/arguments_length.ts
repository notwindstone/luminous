// @native count opt both maybeWritten deleted inArrow other nested async_count gen_count main
// A function that reads its `arguments` only as `arguments.length` gets the
// count, not the object; any other use (written, deleted, an arrow's, the
// object itself) keeps the object.
function count(a?: number, b?: number, c?: number): number {
  return arguments.length;
}
function opt(fd: number, buffer: unknown, offset?: unknown): string {
  if (arguments.length <= 2 || typeof offset === "object") return "options " + arguments.length;
  return "positional " + arguments.length;
}
function both(a?: number): string {
  return arguments.length + ":" + (arguments as any)[0];
}
function maybeWritten(a?: number): number {
  if (a === 1) (arguments as any).length = 7;
  return arguments.length;
}
function deleted(a?: number): string {
  delete (arguments as any).length;
  return String(arguments.length);
}
function inArrow(a?: number): number {
  const f = () => arguments.length;
  return f();
}
function other(a?: number): string {
  return typeof arguments + arguments.length;
}
function nested(a?: number): number {
  function inner(): number {
    return arguments.length;
  }
  return arguments.length * 10 + inner();
}
async function async_count(a?: number, b?: number): Promise<number> {
  await null;
  return arguments.length;
}
function* gen_count(a?: number): Generator<number> {
  yield arguments.length;
  yield arguments.length + 1;
}

async function main() {
  const fns: any[] = [count, opt, both, maybeWritten, deleted, inArrow, other, nested];
  for (const f of fns) {
    console.log(f.name, f(), f(1), f(1, 2), f(undefined, undefined, undefined), f.apply(null, [1, 2, 3, 4]), f.call(null, 0));
  }
  console.log(await async_count(), await async_count(1, 2, 3));
  console.log([...gen_count()].join(), [...(gen_count as any)(1, 2)].join());
  console.log(opt(1, 2), opt(1, 2, {}), opt(1, 2, 3, 4 as never));
}
main();
