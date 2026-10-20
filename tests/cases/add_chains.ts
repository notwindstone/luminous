// @native paths mixed objects numbersFirst throwing sums codes
// Chains of `+` on values not known to be strings (`a + "/" + b`) are added
// by one call: concatenated at once when they are strings and primitives,
// else pair by pair as `+` does (objects by valueOf, not toString; numbers
// first are summed), and integer sums of character codes computed as
// integers.

function join(a: any, b: any): any {
  return a + "/" + b;
}

function join4(a: any, b: any, c: any): any {
  return a + "-" + b + "-" + c;
}

function paths(): string {
  const r: string[] = [];
  const parts = ["usr", "", "local", "λib", "x".repeat(600)];
  for (const a of parts) for (const b of parts) r.push(String(join(a, b).length) + join(a, b).slice(0, 8));
  r.push(join4("a", "b", "c"), join4("", "", ""), join4("λ", 1, 2.5));
  return r.join(",");
}

function mixed(): string {
  const r: string[] = [];
  const values: any[] = [1, -0, 2.5, NaN, true, null, undefined, "s", "λ", 2 ** 40];
  for (const a of values) for (const b of values) r.push(join(a, b));
  return r.join("|");
}

function objects(): string {
  const v = { valueOf: () => 42, toString: () => "str" };
  const t = { toString: () => "only" };
  const d = new Date(0);
  const arr = [1, 2];
  return [join(v, "x"), join("x", v), join4(v, t, arr), join(d, 1).length, join4(1, v, 2), join(arr, arr)].join("|");
}

function numbersFirst(a: any, b: any): any {
  return a + b + "!" + a;
}

function throwing(): string {
  const bad = {
    valueOf() {
      throw new Error("no");
    },
  };
  const r: string[] = [];
  try {
    r.push(join("a", bad));
  } catch (e) {
    r.push((e as Error).message);
  }
  try {
    r.push(join(Symbol("s"), "a"));
  } catch (e) {
    r.push((e as Error).constructor.name);
  }
  try {
    r.push(join4("a", 1, Symbol("t")));
  } catch (e) {
    r.push((e as Error).constructor.name);
  }
  return r.join(",");
}

function sums(s: string): string {
  let h = 0;
  let g = 0;
  for (let i = 0; i < s.length; i++) {
    h = (h + s.charCodeAt(i)) | 0;
    g = (g * 31 + s.charCodeAt(i)) | 0;
  }
  // (Past the end: NaN, which | 0 makes 0.)
  const past = (h + s.charCodeAt(s.length)) | 0;
  const sub = (s.charCodeAt(0) - h) | 0;
  return [h, g, past, sub].join(",");
}

function codes(s: string): string {
  let n = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    n = (n + c - 48) | 0;
    if (((c + 1) | 0) === 0x3c) n++;
  }
  return String(n);
}

console.log(paths());
console.log(mixed());
console.log(join(BigInt(10), "x"), join4(BigInt(-3), BigInt(4), "y"));
console.log(objects());
console.log(numbersFirst(1, 2), numbersFirst("1", 2), numbersFirst(1, "2"), numbersFirst(null, true));
console.log(throwing());
console.log(sums("/usr/local/lib/share/file.txt"), sums(""), sums("λθ\u{1F600}".repeat(50)));
console.log(codes("0123456789;;<<"), codes("λ".repeat(1000)));
