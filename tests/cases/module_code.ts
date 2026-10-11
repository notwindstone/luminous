// @native readShared readLater pairSum bumpCounter
// @interpreted slow arrowSlow wrapsInterpreted
type int = number;

function readShared(): number {
  return shared * 2;
}
function readLater(): string {
  try {
    return String(later);
  } catch (e) {
    return (e as Error).constructor.name;
  }
}
console.log("before", readLater(), typeof hoisted);

// Locals of the module code, and bindings shared with functions.
let local = 0;
for (let i = 0; i < 1000; i++) local += i;
let shared = local;
console.log("shared", readShared(), local);
var hoisted = "set";
console.log("hoisted", hoisted);

const [first, second] = [3, 4];
function pairSum(): number {
  return first + second;
}
console.log("pair", pairSum());

// Closures over module locals and over cells.
let captured = 1;
const adders = [1, 2, 3].map((x) => () => x + captured + shared);
captured = 10;
console.log("closures", adders.map((f) => f()).join(","));

// A statement that stays interpreted (a tagged template) splits the code
// around it; the bindings it shares become cells.
let split = 1;
const tagged = String.raw`${split}+${split + 1}`;
split *= 2;
console.log("split", split, tagged);

class Box {
  constructor(public v: number) {}
  twice(): number {
    return this.v * 2;
  }
}
let boxes = 0;
for (const b of [new Box(1), new Box(2)]) boxes += b.twice();
console.log("boxes", boxes);

// Control flow at the top level.
outer: for (let i = 0; i < 5; i++) {
  for (let j = 0; j < 5; j++) {
    if (j === 3) continue outer;
    if (i === 3) break outer;
  }
}
let caught = "";
try {
  throw new Error("top-level throw");
} catch (e) {
  caught = (e as Error).message;
} finally {
  caught += "!";
}
console.log("control", caught);

let counter: int = 2147483646;
counter++;
counter += 1;
function bumpCounter(): number {
  counter++;
  return counter;
}
console.log("int", counter, bumpCounter());

const later = "initialized";
console.log("after", readLater());

export const exported = local + 1;
console.log("exported", exported);

interpreted function slow(x: number): number {
  return x + 1;
}
const arrowSlow = interpreted (x: number): number => x * 3;
function wrapsInterpreted(): number {
  const inner = interpreted function (): number {
    return 7;
  };
  return inner();
}
console.log("interpreted", slow(1), arrowSlow(2), wrapsInterpreted());
const interpreted = "an ordinary identifier";
console.log(interpreted);

// A type and a value with the same name: one symbol, whose span is the
// type's. The value, declared by native module code and used by an
// interpreted function, lives in a cell.
type Shared = { n: number; tag: string };
const Shared: Shared = { n: 1, tag: "shared" };
Shared.n += 41;
interpreted function readShared2(): string {
  return Shared.tag + " " + Shared.n;
}
console.log(readShared2());
