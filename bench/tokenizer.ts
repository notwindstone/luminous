// A tokenizer over generated source text: character codes, substrings,
// a Map of identifier counts, and an array of token records.
type Token = { kind: number; start: number; end: number };

function isIdentStart(c: number): boolean {
  return (c >= 97 && c <= 122) || (c >= 65 && c <= 90) || c === 95;
}

function isDigit(c: number): boolean {
  return c >= 48 && c <= 57;
}

function tokenize(src: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src.charCodeAt(i);
    if (c === 32 || c === 10) {
      i++;
    } else if (isIdentStart(c)) {
      const start = i;
      while (i < n && (isIdentStart(src.charCodeAt(i)) || isDigit(src.charCodeAt(i)))) i++;
      tokens.push({ kind: 1, start, end: i });
    } else if (isDigit(c)) {
      const start = i;
      while (i < n && isDigit(src.charCodeAt(i))) i++;
      tokens.push({ kind: 2, start, end: i });
    } else {
      tokens.push({ kind: 3, start: i, end: i + 1 });
      i++;
    }
  }
  return tokens;
}

function makeSource(lines: number): string {
  const words = ["let", "value", "count", "total", "index", "result", "item", "node"];
  const parts: string[] = [];
  for (let i = 0; i < lines; i++) {
    const a = words[i % words.length];
    const b = words[(i * 7) % words.length];
    parts.push(`${a}${i % 13} = ${b} + ${i * 31} * (${a} - ${i % 97});`);
  }
  return parts.join("\n");
}

const start = Date.now();
const src = makeSource(40000);
let idents = 0;
let numbers = 0;
const counts = new Map<string, number>();
for (let round = 0; round < 5; round++) {
  const tokens = tokenize(src);
  for (const t of tokens) {
    if (t.kind === 1) {
      idents++;
      const name = src.substring(t.start, t.end);
      counts.set(name, (counts.get(name) ?? 0) + 1);
    } else if (t.kind === 2) {
      numbers++;
    }
  }
}
console.log(`identifiers: ${idents}, numbers: ${numbers}, distinct: ${counts.size}, value0: ${counts.get("value0")}`);
console.log(`elapsed: ${Date.now() - start} ms`);
