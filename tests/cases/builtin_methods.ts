// @native codes edge isDigit flag countDigits gated methods arrays callbacks patched
// Methods of strings read from String.prototype through inline caches,
// `charCodeAt` inline while it is the original, builtins called directly,
// callbacks entered from builtins, and direct calls with JS values for
// `number` and `boolean` parameters (checked at the call).

function codes(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return h;
}

function edge(s: string): string {
  const out: unknown[] = [];
  out.push(s.charCodeAt(-1), s.charCodeAt(s.length), s.charCodeAt(), s.charCodeAt(1.7), s.charCodeAt(-0.5));
  out.push(s.charCodeAt(NaN), s.charCodeAt("1" as unknown as number), s.charCodeAt(true as unknown as number));
  out.push(s.charCodeAt(2 ** 40), s.charCodeAt(-(2 ** 40)), s.charCodeAt(Infinity));
  const w = "héllo€" + s;
  out.push(w.charCodeAt(1), w.charCodeAt(5), w.charCodeAt(6));
  const sl = s.slice(1);
  out.push(sl.charCodeAt(0), sl.charCodeAt(5));
  const o = {
    charCodeAt(i: number) {
      return "obj" + i;
    },
  };
  out.push((o as unknown as string).charCodeAt(3));
  const boxed = new String("qr");
  out.push(boxed.charCodeAt(1));
  const n = 42 as unknown as string;
  try {
    out.push(n.charCodeAt(0));
  } catch (e) {
    out.push(String(e));
  }
  return out.join(",");
}

function isDigit(c: number): boolean {
  return c >= 48 && c <= 57;
}

function flag(b: boolean): string {
  return b ? "yes" : "no";
}

function countDigits(s: string): number {
  let n = 0;
  for (let i = 0; i < s.length; i++) if (isDigit(s.charCodeAt(i))) n++;
  return n;
}

function gated(xs: unknown[]): string {
  // JS values for unboxed parameters: numbers and booleans call directly,
  // others go through the function object (and the interpreter).
  return xs.map((x) => String(isDigit(x as number)) + ":" + flag(x as boolean)).join(" ");
}

function methods(s: string): string {
  const parts = s.split(",");
  const out: string[] = [];
  for (const p of parts) {
    out.push(p.trim().toUpperCase(), String(p.indexOf("b")), p.slice(1, 3), String(p.startsWith(" ")));
  }
  return out.join("|");
}

function arrays(n: number): string {
  const xs: number[] = [];
  for (let i = 0; i < n; i++) xs.push(i * 3);
  const m = new Map<string, number>();
  for (let i = 0; i < n; i++) m.set("k" + (i % 4), (m.get("k" + (i % 4)) ?? 0) + i);
  const s = new Set<number>();
  for (const x of xs) s.add(x % 7);
  return [xs.length, xs.join("-"), [...m.entries()].join(";"), s.size, xs.indexOf(9)].join(" ");
}

function callbacks(n: number): string {
  const xs: { id: number; v: number }[] = [];
  for (let i = 0; i < n; i++) xs.push({ id: i, v: (i * 37) % 11 });
  const sorted = xs.slice().sort((a, b) => b.v - a.v || a.id - b.id);
  const total = xs.map((x) => x.v * 2).filter((v) => v > 4).reduce((a, b) => a + b, 0);
  return sorted.map((x) => x.id).join(",") + " " + total + " " + xs.every((x) => x.v < 11);
}

function patched(): string {
  const out: unknown[] = [];
  const proto = String.prototype as unknown as Record<string, unknown>;
  const origCode = String.prototype.charCodeAt;
  const origSlice = String.prototype.slice;
  const origPush = Array.prototype.push;
  for (let round = 0; round < 6; round++) {
    if (round === 1) proto.charCodeAt = (i: number) => -i;
    if (round === 2) proto.charCodeAt = origCode;
    if (round === 3) proto.extra = 1;
    if (round === 4) {
      proto.slice = () => "sliced";
      (Array.prototype as unknown as Record<string, unknown>).push = () => 7;
    }
    if (round === 5) {
      proto.slice = origSlice;
      Array.prototype.push = origPush;
      delete proto.extra;
    }
    const a: number[] = [];
    const r = a.push(round);
    out[out.length] = [codes("abc"), "abc".charCodeAt(1), "abcd".slice(1), a.length, r].join(" ");
  }
  return out.join(",");
}

console.log(codes("hello world"), codes("€uro"), codes(""));
console.log(edge("abc"));
console.log(countDigits("a1b22c333"), countDigits("none"));
console.log(gated([5, 53, "53", null, true, false, 0, undefined, { valueOf: () => 50 }]));
console.log(methods("abc, xbz ,  b,q"));
console.log(arrays(20));
console.log(callbacks(30));
console.log(patched());
console.log(codes("after"), edge("xyz"));
