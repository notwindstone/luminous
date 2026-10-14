// @native plain
// @interpreted literal template keys
// Native code keeps literal strings as UTF-8, which has no lone surrogates:
// a function with such a literal stays interpreted (and module code only
// the statement with it), so the string is exact either way.

function literal(): string {
  const s = "a\uD800b";
  return `${s.length} ${s.charCodeAt(1).toString(16)} ${"\uDC00".length}`;
}

function template(): string {
  const t = `x\uD83Dy`;
  return `${t.length} ${t === "x" + String.fromCharCode(0xd83d) + "y"}`;
}

function keys(): string {
  const o: Record<string, number> = { "\uDBFF": 1 };
  o["\uDFFF"] = 2;
  return Object.keys(o).map((k) => k.charCodeAt(0).toString(16)).join(",");
}

function plain(): string {
  // U+FFFD itself, and a pair that is not lone.
  const s = "�1234 😀";
  return `${s.length} ${s.codePointAt(6)!.toString(16)}`;
}

const top = "\uD800".length;
console.log(literal());
console.log(template());
console.log(keys());
console.log(plain());
console.log(top);
