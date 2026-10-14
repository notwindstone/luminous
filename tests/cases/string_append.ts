// @native build forks uses
// `s += x` onto a string longer than 8KB appends into a buffer with room
// to spare, of which the strings are slices: a string that is not the
// last appended to copies itself into a new buffer when appended to, so
// every string keeps its characters; 8-bit buffers become 16-bit ones for
// wide characters; the results behave as any string (equality, Map keys,
// searching, slicing, splitting, JSON).

function build(n: number, piece: string): string {
  let s = "";
  for (let i = 0; i < n; i++) s += piece + i;
  return s;
}

function forks(): string {
  let a = "x".repeat(9000);
  const b = a + "B";
  const c = a + "C";
  const d = b + "D";
  const e = b + "E";
  a += "A";
  const wide = d + "é€";
  const back = wide + "z";
  const more = e + "ü";
  const again = b + "F";
  return [
    a.slice(-3),
    b.slice(-3),
    c.slice(-3),
    d.slice(-3),
    e.slice(-3),
    wide.slice(-4),
    back.slice(-5),
    more.slice(-3),
    again.slice(-3),
    a.length,
    wide.length,
    String(b < c),
    String(d === b + "D"),
  ].join(",");
}

function uses(): string {
  const s = build(3000, "ab");
  const m = new Map<string, number>([[s, 1]]);
  const t = build(3000, "ab");
  const o: Record<string, number> = {};
  o[s] = 2;
  return [
    s.length,
    s === t,
    m.get(t),
    o[t],
    s.indexOf("2999"),
    s.split("ab").length,
    s.slice(9000, 9010),
    JSON.stringify(s.slice(-6)),
    s.charCodeAt(5),
    s.endsWith("2999"),
    (s + "!").length,
    s.replace(/ab1/g, "-").length,
    s.toUpperCase().slice(0, 6),
    JSON.parse(JSON.stringify({ s })).s === s,
  ].join(",");
}

console.log(forks());
console.log(uses());
console.log(build(20000, "<tr>").length, build(500, "é").slice(-8));
