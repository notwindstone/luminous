// @native literals search replace splitting
// @interpreted patchExec restoreExec
// Regular expression literals in native code: each evaluation makes a new
// RegExp (its own `lastIndex`) of a pattern compiled once. A search whose
// pattern starts with a character tries it only where that character is
// (8-bit and 16-bit strings, the `u` flag; not `i` or `y`); a global
// replacement by a string without `$` inserts it as is, while the regexp
// is a standard one (its `exec` and `constructor` the builtins).

function literals(): string {
  const out: string[] = [];
  for (let i = 0; i < 3; i++) {
    const r = /o/g;
    out.push(String(r.lastIndex), String(r.test("foo")), String(r.lastIndex));
  }
  const a = /a\/b[\]\\]c/imsu;
  out.push(a.source, a.flags, String(a.global), String(a.unicode), String(/x/ === /x/), String(/x/d.hasIndices));
  out.push(String(/[é-ÿ]/.test("café")), String(/\p{L}+/u.exec("123abc")![0]));
  return out.join(" ");
}

function search(): string {
  const out: string[] = [];
  out.push(String(/ab+c/.exec("abxabbbcab")!.index));
  const m = /a(b)?(c)/.exec("abaxacab")!;
  out.push(String(m.index), String(m[1]), m[2]);
  out.push(String(/q/.test("abc")), String(/ÿ/.test("aÿb")), String(/Ā/.test("abc")), String(/Ā/.test("aĀb")));
  out.push(String("xyzé€xyz€".search(/€x/)), String("😀a😀b".search(/a/u)), String("😀a😀b".match(/b/u)!.index));
  out.push(String(/b/y.test("ab")), String(/B/i.test("ab")), String(/^b/m.test("a\nb")), String(/c$/.test("abc")));
  const g = /a./g;
  const seen: string[] = [];
  let r: RegExpExecArray | null;
  while ((r = g.exec("a1 b2 a3 aa a")) !== null) seen.push(r[0] + "@" + r.index + ":" + g.lastIndex);
  out.push(seen.join(","), String(g.lastIndex));
  out.push([..."x1y22x333".matchAll(/x(\d+)/g)].map((m) => m[1] + "@" + m.index).join(","));
  out.push(String(/(a)|b/.exec("cb")![1]), String(/a(?=b)/.exec("acab")!.index), String(/(?<=a)b/.exec("cbab")!.index));
  return out.join(" ");
}

function replace(): string {
  const out: string[] = [];
  const s = "K1 k10 K12 xK1y";
  out.push(s.replace(/K1/g, "X"), s.replace(/K1/, "X"), s.replace(/K1/g, ""), s.replace(/K1/g, "[$&]"));
  out.push(s.replace(/K(1)/g, "<$1>"), s.replace(/K1/g, (m) => m.toLowerCase()), s.replace(/k1/gi, "_"));
  out.push("aaa".replace(/a*/g, "-"), "abc".replace(/(?:)/g, "."), "ĀbĀb".replace(/b/g, "é"), "😀b😀".replace(/b/gu, "c"));
  const re = /o/g;
  re.lastIndex = 5;
  out.push("foo boo".replace(re, "0"), String(re.lastIndex));
  return out.join(" | ");
}

function splitting(): string {
  return ["a1b22c333d".split(/\d+/).join(","), "a, b,c".split(/\s*,\s*/).join("|"), "x".split(/y/).join(",")].join(" ");
}

let savedExec: unknown;
function patchExec(): void {
  "use no aot";
  savedExec = RegExp.prototype.exec;
  RegExp.prototype.exec = function (this: RegExp, s: string) {
    const r = (savedExec as (s: string) => RegExpExecArray | null).call(this, s);
    if (r) r[0] = r[0] + "?";
    return r;
  };
}
function restoreExec(): void {
  "use no aot";
  RegExp.prototype.exec = savedExec as typeof RegExp.prototype.exec;
}

console.log(literals());
console.log(search());
console.log(replace());
console.log(splitting());
patchExec();
console.log("ab ab".replace(/b/g, "c"));
restoreExec();
console.log("ab ab".replace(/b/g, "c"));
