// @native plain hasSpace countClass lastDigit edges pick classes
// Characters' codes kept in locals and compared with constants as
// integers (-1 standing for NaN past the end), regions returning booleans,
// and conditions of `&&`, `||`, `!` and `?:` as branches.

// A region returning a boolean, a code compared with many constants.
function plain(s: string): boolean {
  const len = s.length;
  for (let i = 0; i < len; i++) {
    const c = s.charCodeAt(i);
    if (c <= 0x20 || c >= 0x7f || c === 0x22 || c === 0x25 || c === 0x3c || c === 0x3e || c === 0x5c || c === 0x7b) {
      return false;
    }
  }
  return true;
}

function hasSpace(s: string, from: number): boolean {
  for (let i = from; i < s.length; i++) {
    if (s.charCodeAt(i) === 32) return true;
  }
  return false;
}

// A code local that starts as a constant, `!` and `?:` in tests.
function countClass(s: string): number {
  let c = 0;
  let n = 0;
  for (let i = 0; i < s.length; i++) {
    c = s.charCodeAt(i);
    if (!(c >= 0x30 && c <= 0x39) ? c === 0x2d || c === 0x2b : c !== 0x30) n++;
  }
  return n * 1000 + c;
}

// Arithmetic on the code, the loop leaving past the end.
function lastDigit(s: string): number {
  let i = 0;
  let v = -1;
  while (true) {
    const c = s.charCodeAt(i);
    if (!(c >= 48 && c <= 57)) {
      if (c !== c) return v;
      i++;
      continue;
    }
    v = c - 48;
    i++;
  }
}

// Each comparison with a code past the end (NaN), and inside: a bit each.
function edges(s: string, i: number): number {
  const c = s.charCodeAt(i);
  const d = s.charCodeAt(i + 1);
  let r = d === c ? 16384 : 0;
  if (c === -1) r += 1;
  if (c !== -1) r += 2;
  if (c > -1) r += 4;
  if (c >= -1) r += 8;
  if (c < 0) r += 16;
  if (c <= -1) r += 32;
  if (c < 200) r += 64;
  if (c >= 0) r += 128;
  if (c === 97) r += 256;
  if (c !== 97) r += 512;
  if (-1 === c) r += 1024;
  if (0 > c) r += 2048;
  if (0 <= c) r += 4096;
  if (c !== c) r += 8192;
  return r;
}

function pick(s: string, i: number, j: number): number {
  const a = s.charCodeAt(i);
  const b = s.charCodeAt(j);
  if ((a === 120 && b !== 121) || (!(a === b) && b === 122)) return 1;
  if (a < b === b < a) return 2;
  return a > b ? 3 : 4;
}

function classes(s: string): number {
  let out = 0;
  for (let i = 0; i <= s.length; i++) {
    const c = s.charCodeAt(i);
    const k = c >= 0x61 && c <= 0x7a ? 1 : c >= 0x30 && c <= 0x39 ? 2 : c === c ? 3 : 4;
    out = (out * 4 + k) % 1000003;
  }
  return out;
}

const samples = ["http://example.com/a?b#c", "has space", "quote\"d", "{brace", "", "ünï", "日本", "tab\there", "x".repeat(40)];
for (const s of samples) {
  const rope = s + "".concat("!", s);
  console.log(JSON.stringify(s), plain(s), plain(rope), hasSpace(s, 0), hasSpace(s, -3), hasSpace(s, 100));
  console.log(countClass(s), countClass(rope), lastDigit(s), lastDigit(s + "7z"), classes(s));
  console.log(edges(s, 0), edges(s, -1), edges(s, s.length), edges(s, 1.5), edges(s, NaN), edges(s, 1e10));
  console.log(pick(s, 0, 1), pick(s, 1, 0), pick(s, -1, 0), pick(s, 0, 99), pick("xz", 0, 1), pick("aa", 0, 1));
}
console.log(countClass("0123-+9"), countClass("+"), lastDigit("a1b22c"), plain(String.fromCharCode(0x7e)), plain("\x7f"));
