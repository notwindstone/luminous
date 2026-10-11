// @native fill fillAt frozen sealed setter subclass protoSetter sparse
function fill(n: number): number[] {
  const xs: number[] = new Array(n);
  for (let i = 0; i < n; i++) xs[i] = i * 0.5;
  return xs;
}

function fillAt(xs: number[], from: number, n: number): string {
  for (let i = from; i < from + n; i++) xs[i] = i;
  return xs.length + ":" + xs.join(",");
}

function frozen(): string {
  const xs: number[] = [1, 2];
  Object.freeze(xs);
  try {
    xs[2] = 3;
  } catch (e) {
    return "threw " + (e as Error).constructor.name + " " + xs.length;
  }
  return "no throw " + xs.length;
}

function sealed(): string {
  const xs: number[] = [1, 2];
  Object.preventExtensions(xs);
  let out = "";
  try {
    xs[2] = 3;
    out += "no throw";
  } catch (e) {
    out += (e as Error).constructor.name;
  }
  xs[1] = 5;
  return out + " " + xs.join(",");
}

function setter(): string {
  const xs: number[] = [1, 2];
  let got = -1;
  Object.defineProperty(xs, "2", { set: (v: number) => (got = v), configurable: true });
  xs[2] = 9;
  return got + " " + xs.length;
}

class Tracked extends Array<number> {}

function subclass(): string {
  const xs = new Tracked();
  for (let i = 0; i < 4; i++) xs[i] = i;
  return xs.length + " " + (xs instanceof Tracked) + " " + xs.join(",");
}

function protoSetter(): string {
  let got = "";
  Object.defineProperty(Array.prototype, "1", {
    set: (v: number) => (got += "set" + v),
    configurable: true,
  });
  const xs: number[] = [7];
  xs[1] = 8;
  const r = got + " " + xs.length;
  delete (Array.prototype as unknown as Record<string, unknown>)["1"];
  return r;
}

function sparse(): string {
  const xs: number[] = [];
  xs[0] = 1;
  xs[2] = 3;
  xs[1] = 2;
  xs[3] = 4;
  return xs.length + " " + xs.join(",");
}

console.log(fill(5).join(","), fill(0).length);
console.log(fillAt([], 0, 3), fillAt([1], 1, 2), fillAt([1, 2, 3], 1, 4));
console.log(frozen());
console.log(sealed());
console.log(setter());
console.log(subclass());
console.log(protoSetter());
console.log(sparse());
