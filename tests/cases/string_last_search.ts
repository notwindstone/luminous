// @native last positions wide
// lastIndexOf: of characters and longer needles, from the end or an int32
// position (clamped; others converted), empty needles, needles longer than
// the string, 16-bit strings and needles, and sliced strings.

const hay = "the quick brown fox jumps over the lazy dog; the quick brown cat naps under the busy log";

function last(s: string): string {
  const out: number[] = [];
  for (let len = 0; len <= 12; len++) {
    for (let start = 0; start + len <= s.length && start < 80; start += 7) {
      const needle = s.slice(start, start + len);
      out.push(s.lastIndexOf(needle), s.lastIndexOf(needle, start), s.lastIndexOf(needle, start - 1));
    }
  }
  let h = 0;
  for (const n of out) h = (h * 31 + n) | 0;
  return h + " " + out.length + " " + out.slice(0, 15).join(",");
}

function positions(): string {
  const s = "aaabaaabaaab";
  const r: number[] = [];
  for (const from of [-5, 0, 1, 3, 4, 11, 12, 13, 100]) {
    r.push(s.lastIndexOf("aab", from), s.lastIndexOf("", from), s.lastIndexOf("b", from), s.lastIndexOf("aaabaaabaaabX", from));
  }
  r.push(s.lastIndexOf("aab", 2.5 as number), s.lastIndexOf("aab", NaN), s.lastIndexOf("aab", -Infinity), s.lastIndexOf("aab", Infinity));
  r.push("".lastIndexOf(""), "".lastIndexOf("a"), "abc".lastIndexOf("abcd"), "abc".lastIndexOf("a", 0), "abc".lastIndexOf("c", 1));
  r.push(s.lastIndexOf("b", "7" as any), s.lastIndexOf(98 as any), "a,b,c".lastIndexOf(","), "/usr/lib/".lastIndexOf("/"));
  return r.join(",");
}

function wide(): string {
  const w = hay.replace("fox", "føx").replace("dog", "dθg") + " θ";
  return [w.lastIndexOf("θ"), w.lastIndexOf("dθg"), w.lastIndexOf("ø", 20), w.lastIndexOf("o"), hay.lastIndexOf("θ"), w.lastIndexOf("θ ", 100)].join(",");
}

console.log(last(hay));
console.log(last(hay.repeat(20).slice(5, 1900)));
console.log(last(hay.replace("o", "ô")));
console.log(positions());
console.log(wide());
