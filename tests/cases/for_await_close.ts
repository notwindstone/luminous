// @native ticks thrown broken rejected nonObject leftEarly continued main
// `for await` leaving by an exception closes the iterator as the spec's
// AsyncIteratorClose closes it on a throw completion: `return` called and
// its result awaited, what they throw ignored, before the exception goes on.
const log: string[] = [];

async function ticks(n: number): Promise<void> {
  for (let i = 0; i < n; i++) await null;
}

interface Opts {
  throws?: boolean;
  rejects?: boolean;
  nonObject?: boolean;
}

function source(name: string, opts: Opts): AsyncIterable<number> {
  let i = 0;
  const it = {
    [Symbol.asyncIterator]() {
      return it;
    },
    next(): Promise<IteratorResult<number>> {
      i++;
      return Promise.resolve({ value: i, done: i > 3 });
    },
    async return(): Promise<any> {
      log.push(`${name}: return`);
      if (opts.throws) throw new Error(`${name} return threw`);
      await ticks(3);
      if (opts.rejects) throw new Error(`${name} return rejected`);
      log.push(`${name}: settled`);
      return opts.nonObject ? 42 : { done: true };
    },
  };
  return it;
}

async function thrown(): Promise<string> {
  try {
    for await (const x of source("thrown", {})) {
      if (x === 2) throw new Error("body threw");
    }
  } catch (e) {
    return (e as Error).message;
  }
  return "no error";
}

async function broken(): Promise<string> {
  try {
    for await (const _ of source("broken", { throws: true })) throw new Error("body threw");
  } catch (e) {
    return (e as Error).message;
  }
  return "no error";
}

async function rejected(): Promise<string> {
  try {
    for await (const _ of source("rejected", { rejects: true })) throw new Error("body threw");
  } catch (e) {
    return (e as Error).message;
  }
  return "no error";
}

async function nonObject(): Promise<string> {
  try {
    for await (const _ of source("nonObject", { nonObject: true })) throw new Error("body threw");
  } catch (e) {
    return (e as Error).message;
  }
  return "no error";
}

async function leftEarly(): Promise<string> {
  for await (const x of source("leftEarly", {})) {
    if (x === 2) return "returned " + x;
  }
  return "end";
}

async function continued(): Promise<string> {
  let n = 0;
  outer: for await (const _ of source("outer", {})) {
    for await (const y of source("inner", {})) {
      if (y === 2) continue outer;
      n++;
    }
  }
  return "continued " + n;
}

async function main(): Promise<void> {
  for (const f of [thrown, broken, rejected, nonObject, leftEarly, continued]) {
    log.push(`${f.name} => ${await f()}`);
  }
  console.log(log.join("\n"));
}

main();
