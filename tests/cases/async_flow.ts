// @native finallyPaths loops cases nestedFinally
const log: string[] = [];
const tick = (v: unknown) => Promise.resolve(v);

async function finallyPaths(mode: string): Promise<string> {
  try {
    log.push("try " + mode);
    if (mode === "throw") throw new Error("boom");
    if (mode === "return") return "returned " + (await tick(1));
    log.push("normal");
  } catch (e) {
    log.push("caught " + (e as Error).message);
    if (mode === "throw") throw new TypeError("rethrown");
  } finally {
    log.push("finally start " + mode);
    await tick(0);
    log.push("finally after await " + (await tick(mode)));
  }
  return "fell through " + mode;
}

async function nestedFinally(): Promise<number> {
  let n = 0;
  try {
    try {
      return n;
    } finally {
      n += (await tick(10)) as number;
      log.push("inner finally " + n);
    }
  } finally {
    for (let i = 0; i < 2; i++) {
      n += (await tick(1)) as number;
    }
    log.push("outer finally " + n);
  }
}

async function loops(): Promise<string> {
  const out: number[] = [];
  for (let i = 0; i < 3; i += (await tick(1)) as number) out.push(i);
  let j = 0;
  do {
    out.push(100 + j);
    j++;
    if (j === 1) continue;
  } while ((await tick(j)) < 3);
  let k = 0;
  for (; ; k = (await tick(k + 1)) as number) {
    if (k > 2) break;
    out.push(200 + k);
  }
  const fns: (() => number)[] = [];
  for (let m = 0; m < 3; m = (await tick(m + 1)) as number) fns.push(() => m);
  for (let q = 0; q < fns.length; q++) out.push(fns[q]());
  return out.join(",");
}

async function cases(x: unknown): Promise<string> {
  const seen: string[] = [];
  switch (x) {
    case await tick("a"):
      seen.push("a");
    case "b":
      seen.push("b");
      break;
    default:
      seen.push("default");
    case (seen.push("eval c"), await tick("c")):
      seen.push("c");
      break;
    case await tick("d"):
      seen.push("d");
  }
  return seen.join(" ");
}

async function main() {
  for (const m of ["normal", "throw", "return"]) {
    try {
      log.push("result " + (await finallyPaths(m)));
    } catch (e) {
      log.push("error " + (e as Error).name + " " + (e as Error).message);
    }
  }
  log.push("nested " + (await nestedFinally()));
  log.push(await loops());
  for (const x of ["a", "b", "c", "d", "z"]) log.push(await cases(x));
  console.log(log.join("\n"));
}
main();
