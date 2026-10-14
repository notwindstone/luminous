// @native searches positions affixes equality keys
// Searching and comparing strings: indexOf from int32 positions,
// includes / startsWith / endsWith with string arguments, and equality of
// strings up to 16 characters compared inline: needles of every length
// around the word sizes, partial matches, positions past either end,
// empty needles, 16-bit and sliced (over 1 KiB) strings, and Map keys.

const hay = "the quick brown fox jumps over the lazy dog; the quick brown cat naps under the busy log";

function searches(s: string): string {
  const out: number[] = [];
  for (let len = 0; len <= 20; len++) {
    for (let start = 0; start + len <= 60; start += 9) {
      const needle = s.slice(start, start + len);
      out.push(s.indexOf(needle), s.indexOf(needle, start + 1), s.includes(needle) ? 1 : 0);
    }
  }
  let h = 0;
  for (const n of out) h = (h * 31 + n) | 0;
  return h + " " + out.length + " " + out.slice(0, 12).join(",");
}

function positions(): string {
  const s = "aaabaaabaaab";
  const r: number[] = [];
  for (const from of [-5, 0, 1, 3, 4, 11, 12, 13, 100]) {
    r.push(s.indexOf("aab", from), s.indexOf("", from), s.indexOf("b", from), s.indexOf("aaabaaabaaabX", from));
  }
  r.push(s.indexOf("aab", 2.5 as number), s.indexOf("aab", NaN), s.indexOf("aab", -Infinity));
  r.push("".indexOf(""), "".indexOf("a"), "abc".indexOf("abcd"), "abc".indexOf("c", 2), "abc".indexOf("c", 3));
  return r.join(",");
}

function affixes(words: string[]): string {
  const r: string[] = [];
  for (const w of words) {
    for (const a of ["", "a", "ab", "abc", "bc", "c", "abcdefghijklmnopq", "pq", "xabc", w, w + "!"]) {
      r.push((w.startsWith(a) ? "S" : "-") + (w.endsWith(a) ? "E" : "-") + (w.includes(a) ? "I" : "-"));
    }
  }
  return r.join("");
}

function equality(): string {
  const base = "abcdefghijklmnopqrstuvwxyz";
  let same = 0;
  let differ = 0;
  for (let len = 0; len <= 20; len++) {
    const a = base.slice(0, len);
    const b = (base + "!").slice(0, len);
    if (a === b) same++;
    for (let i = 0; i < len; i++) {
      const c = a.slice(0, i) + "#" + a.slice(i + 1);
      if (a === c) same += 100;
      else differ++;
      if (c.slice(0, i) + base[i] + c.slice(i + 1) === a) same++;
    }
  }
  const wide = "λ" + base;
  return same + " " + differ + " " + (wide.slice(1) === base) + " " + (wide === "λ" + base) + " " + (wide.slice(0, 5) === "λabcd");
}

function keys(n: number): string {
  const m = new Map<string, number>();
  const words = ["user", "path", "pat", "paths", "status", "statue", "latency", "bytes", "bytez", "a", "b", "", "abcdefghijklmnop", "abcdefghijklmnoq", "abcdefghijklmnopq"];
  for (let i = 0; i < n; i++) {
    const w = words[i % words.length];
    const k = (w + "|").slice(0, w.length);
    m.set(k, (m.get(k) ?? 0) + 1);
  }
  return [...m.entries()].map(([k, v]) => k + "=" + v).join(" ");
}

const big = hay.repeat(20);
const sliced = big.slice(3, 1900);
const wideHay = hay.replace("fox", "føx").replace("dog", "dθg");
console.log(searches(hay));
console.log(searches(sliced));
console.log(searches(wideHay));
console.log(positions());
console.log(affixes(["abc", "abcdefghijklmnopq", "", "xabcabc", sliced.slice(0, 40), "λabc"]));
console.log(equality());
console.log(keys(200));
