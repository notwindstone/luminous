// @native step tokens scan firstWide early
// Module-level literal consts are folded unless code that runs before
// their declaration can reach them; a switch on a number against numeric
// constants compares numbers; a loop scanning a string may return any
// value from inside (computed once the specialized loop is left).

const seen: string[] = [];
seen.push("module code ran"); // (Code runs before the consts below.)

// Reached before its declaration (through `early`), so not folded: the
// call throws (TDZ), natively as interpreted.
function early(): string {
  try {
    return String(LATE);
  } catch (e) {
    return (e as Error).constructor.name;
  }
}
const before = early();
const LATE = 7;

const START = 0;
const WORD = 1;
const SPACE = 2;

function step(state: number, c: number): number {
  switch (state) {
    case START:
      return c === 32 ? SPACE : WORD;
    case WORD:
      return c === 32 ? SPACE : WORD;
    case SPACE:
      return c === 32 ? SPACE : WORD;
    default:
      return -1;
  }
}

function tokens(s: string): number {
  let state = START;
  let n = 0;
  for (let i = 0; i < s.length; i++) {
    const next = step(state, s.charCodeAt(i));
    if (next === WORD && state !== WORD) n++;
    state = next;
  }
  return n;
}

function scan(v: unknown): string {
  const s = String(v);
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdfff) return s.toWellFormed();
  }
  return s;
}

function firstWide(s: string): string | null {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c > 0xff) return s.slice(i);
  }
  return null;
}

console.log(seen[0], before, LATE);
console.log(tokens("  the quick  brown fox "), tokens(""), step(9, 1));
console.log(scan("plain") === "plain", scan("a" + String.fromCharCode(0xd800) + "b").charCodeAt(1).toString(16));
console.log(firstWide("abc"), firstWide("abĀcd"));
