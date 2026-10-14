// @native isWord isDigitCode scanWord skipDigits sumCodes firstUpper pairs classify scanAll
// @interpreted patchCharCodeAt restoreCharCodeAt
// Speculative regions over strings: loops reading characters of a flat
// string with the builtin `charCodeAt` run as integer code behind one
// guard; ropes, other values and a replaced `charCodeAt` take the generic
// code, with the same results.

function isWord(c: number): boolean {
  return (c >= 97 && c <= 122) || (c >= 65 && c <= 90) || c === 95;
}

function isDigitCode(c: number): boolean {
  return c >= 48 && c <= 57;
}

function scanWord(s: string, i: number): number {
  const n = s.length;
  while (i < n && (isWord(s.charCodeAt(i)) || isDigitCode(s.charCodeAt(i)))) i++;
  return i;
}

function skipDigits(s: string, i: number, n: number): number {
  let j = i;
  while (j < n && isDigitCode(s.charCodeAt(j))) j++;
  return j - i;
}

// Loops with continue and break, a sum carried out of the region.
function sumCodes(s: string, n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const c = s.charCodeAt(i);
    if (c === 32) continue;
    if (c === 33) break;
    total = total + c * (i % 3 === 0 ? 2 : 1);
  }
  return total;
}

function firstUpper(s: string): number {
  let i = 0;
  while (true) {
    const c = s.charCodeAt(i);
    if (c !== c) return -1;
    if (c >= 65 && c <= 90) return i;
    i++;
  }
}

// A break from the region to an enclosing loop.
function pairs(s: string): string {
  const out: number[] = [];
  outer: for (let k = 0; k < 3; k++) {
    let i = k;
    while (i < s.length) {
      const c = s.charCodeAt(i);
      if (c === 35) break outer;
      i = i + 2;
    }
    out.push(i);
  }
  return out.join(",");
}

function classify(s: string, at: number): string {
  const c = s.charCodeAt(at);
  const d = s.charCodeAt(at + 0.75);
  const e = s.charCodeAt(-0.5);
  const f = s.charCodeAt(NaN);
  return [c, d, e, f, s.charCodeAt(s.length), s.charCodeAt(-1)].join(" ");
}

function scanAll(s: string): string {
  const words: string[] = [];
  let i = 0;
  while (i < s.length) {
    const j = scanWord(s, i);
    if (j > i) {
      words.push(s.substring(i, j));
      i = j;
    } else {
      i++;
    }
  }
  return words.join("|");
}

let saved: unknown;
function patchCharCodeAt(): void {
  "use no aot";
  saved = String.prototype.charCodeAt;
  String.prototype.charCodeAt = function (this: string, i: number) {
    return 48;
  };
}

function restoreCharCodeAt(): void {
  "use no aot";
  String.prototype.charCodeAt = saved as typeof String.prototype.charCodeAt;
}

const text = "let x1 = foo_bar + 42; Ünïcode wörds and Δelta 7";
console.log(scanAll(text));
console.log(scanWord(text, 4), skipDigits(text, 19, text.length), skipDigits("123abc", 0, 6));
console.log(sumCodes("ab c!def", 8), sumCodes("hello", 5), sumCodes("", 0), sumCodes("xy", 5));
console.log(firstUpper("abcDef"), firstUpper("abc"), firstUpper("ÿÿZ"));
console.log(pairs("abcdefg"), pairs("ab#cdef"), pairs("a"));
console.log(classify("hello", 1), "|", classify("δx", 0), "|", classify("", 0));

// A rope (concatenation), a String object and a number.
let rope = "";
for (let k = 0; k < 20; k++) rope = rope + "ab" + k;
console.log(scanAll(rope).length, scanWord(rope, 0), sumCodes(rope, rope.length));
const boxed = new String("ab c") as unknown as string;
console.log(scanWord(boxed, 0), sumCodes(boxed, 4), scanWord(12345 as unknown as string, 0));

patchCharCodeAt();
console.log(scanWord("abc def", 0), skipDigits("abc", 0, 3), sumCodes("abc", 3), classify("ab", 0));
restoreCharCodeAt();
console.log(scanWord("abc def", 0), skipDigits("abc", 0, 3), sumCodes("abc", 3), firstUpper("aB"));
