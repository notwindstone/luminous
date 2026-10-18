// @native errorOf attempts sum sumAot nativeWrites
// Frozen arrays keep their elements where a fast array has them (read as
// fast) while every write, delete and definition of one is refused as JS
// refuses it; arrays of an `aot type` freeze and seal as a whole.
type int = number;
aot type P = { x: int };

function errorOf(f: () => unknown): string {
  try {
    const r = f();
    return "ok " + JSON.stringify(r);
  } catch (e) {
    return (e as Error).constructor.name;
  }
}

function attempts(a: any): string {
  const out: string[] = [];
  out.push(errorOf(() => (a[0] = 9)));
  out.push(errorOf(() => (a[a.length] = 9)));
  out.push(errorOf(() => a.push(9)));
  out.push(errorOf(() => a.pop()));
  out.push(errorOf(() => a.shift()));
  out.push(errorOf(() => a.unshift(1)));
  out.push(errorOf(() => a.reverse()));
  out.push(errorOf(() => a.sort()));
  out.push(errorOf(() => a.splice(0, 1)));
  out.push(errorOf(() => a.fill(0)));
  out.push(errorOf(() => a.copyWithin(0, 1)));
  out.push(errorOf(() => (a.length = 0)));
  out.push(errorOf(() => delete a[0]));
  out.push(errorOf(() => Object.defineProperty(a, 0, { value: a[0] })));
  out.push(errorOf(() => Object.defineProperty(a, 0, { value: 123 })));
  out.push(errorOf(() => Object.defineProperty(a, 0, { writable: true })));
  out.push(errorOf(() => Object.defineProperty(a, "extra", { value: 1 })));
  const child = Object.create(a);
  out.push(errorOf(() => (child[0] = 5)));
  return out.join(" ");
}

function sum(a: readonly number[]): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i];
  return s;
}

function sumAot(a: readonly P[]): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i].x;
  return s;
}

function nativeWrites(a: number[], p: P[]): string {
  return [errorOf(() => (a[1] = 7)), errorOf(() => (p[0] = { x: 7 })), errorOf(() => p.push({ x: 8 })), String(a[1]), String(p[0].x)].join(" ");
}

const plain = Object.freeze([1, 2, 3, 4]);
console.log(Object.isFrozen(plain), Array.isArray(plain), JSON.stringify(Object.getOwnPropertyDescriptor(plain, 1)));
console.log(JSON.stringify(Object.getOwnPropertyDescriptor(plain, "length")), Object.keys(plain).length);
console.log(attempts(plain));
console.log(JSON.stringify(plain), sum(plain), [...plain].join(), plain.map((x) => x * 2).join(), plain.indexOf(3), plain.slice(1).join());
console.log(plain.includes(4), plain.join("-"), JSON.stringify(plain.toSorted((x, y) => y - x)), JSON.stringify(plain.toReversed()));
for (const x of plain) if (x === 2) console.log("iterated");
const empty = Object.freeze([] as number[]);
console.log(Object.isFrozen(empty), errorOf(() => (empty as any).push(1)), (empty as any).length);
const named: any = [1, 2];
named.tag = "t";
Object.freeze(named);
console.log(errorOf(() => (named.tag = "u")), named.tag, Object.isFrozen(named));
const sealed = Object.seal([5, 6]);
console.log(errorOf(() => ((sealed as any)[0] = 50)), errorOf(() => (sealed as any).push(1)), errorOf(() => delete (sealed as any)[1]), JSON.stringify(sealed), Object.isSealed(sealed), Object.isFrozen(sealed));

const ps: P[] = [{ x: 1 }, { x: 2 }, { x: 3 }];
Object.freeze(ps);
console.log(Object.isFrozen(ps), JSON.stringify(Object.getOwnPropertyDescriptor(ps, 0)), sumAot(ps), JSON.stringify(ps));
console.log(attempts(ps));
const live: number[] = [1, 2, 3];
const pl: P[] = [{ x: 1 }];
Object.freeze(live);
Object.freeze(pl);
console.log(nativeWrites(live, pl));
const ps2: P[] = [{ x: 1 }, { x: 2 }];
Object.seal(ps2);
ps2[0] = { x: 10 };
console.log(Object.isSealed(ps2), Object.isFrozen(ps2), JSON.stringify(ps2), errorOf(() => ps2.push({ x: 3 })), errorOf(() => delete (ps2 as any)[0]), errorOf(() => (ps2.length = 1)));
const nonExt: P[] = [{ x: 1 }];
Object.preventExtensions(nonExt);
nonExt[0] = { x: 4 };
console.log(errorOf(() => nonExt.push({ x: 5 })), JSON.stringify(nonExt), Object.isFrozen(nonExt));
