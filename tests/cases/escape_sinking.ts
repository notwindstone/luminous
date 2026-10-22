// @native Acc rare identity patchedLater asyncInstances literalsAsync debugLog
// Allocation sinking: an instance (or object literal) that escapes on a
// rare path is made there, and only there: the common path keeps its
// fields in locals, calls methods that only read `this` as clones over
// them, and passes them to functions that only read them. Once made, every
// later use works on the object: identity and mutations are JS's.
const kept: any[] = [];
const log: string[] = [];

class Acc {
  constructor(
    public x: number,
    public y: number,
  ) {}
  // Not inlined (a loop); reads only `this`.
  total(k: number): number {
    let s = 0;
    for (let i = 0; i < k; i++) s += this.x * i + this.y;
    return s;
  }
  bump(): void {
    this.x += 100;
  }
}

function weigh(a: Acc, w: number): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += a.x * w + a.y;
  return s;
}

function rare(n: number, every: number): string {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const a = new Acc(i, 1);
    t += a.total(3) + weigh(a, 2);
    if (i % every === 0) kept.push(a);
    // (Written after it escaped: the kept object sees it.)
    a.y = 5;
    t += a.total(2) + weigh(a, 1);
  }
  return t + " " + kept.length + " " + kept.map((a) => a.x + ":" + a.y).join(",");
}

function identity(): string {
  const a = new Acc(1, 2);
  const b = new Acc(1, 2);
  const s1 = a.total(2);
  kept.push(a);
  return `${s1} ${kept[kept.length - 1] === a} ${kept.includes(b)} ${a === b}`;
}

function patchedLater(n: number): string {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const a = new Acc(i, 2);
    t += a.total(2);
    if (i === 1) {
      (Acc.prototype as any).total = function (this: Acc, k: number) {
        return -this.x * k;
      };
    }
  }
  return String(t);
}

async function tick(): Promise<void> {
  await null;
}

async function asyncInstances(n: number): Promise<string> {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const a = new Acc(i, 3);
    await tick();
    t += weigh(a, 2) + a.x;
    if (i === 2) a.bump();
    await tick();
    t += a.x;
  }
  return String(t);
}

async function literalsAsync(n: number): Promise<string> {
  let t = 0;
  for (let i = 0; i < n; i++) {
    const o = { p: i, q: 2 };
    await tick();
    t += o.p * o.q;
    if (i === n - 1) log.push(JSON.stringify(o));
  }
  return t + " " + log.join(";");
}

function debugLog(debug: boolean): number {
  let t = 0;
  for (let i = 0; i < 5; i++) {
    const a = new Acc(i, i);
    if (debug) log.push("acc " + a.x + "," + a.y);
    t += a.total(2);
  }
  return t;
}

console.log(rare(10, 4), identity());
console.log(debugLog(false), debugLog(true), log.join("; "));
asyncInstances(5)
  .then((r) => console.log("async", r))
  .then(() => literalsAsync(4))
  .then((r) => {
    console.log("literals", r);
    console.log(patchedLater(4));
  });
