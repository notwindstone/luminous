// @native kind same differs ropes
// `===` / `!==` / `switch` against string constants: by pointer, then
// length, then characters; ropes, slices, wide strings and other types.

function kind(k: unknown): string {
  switch (k) {
    case "user":
      return "U";
    case "path":
      return "P";
    case "status":
      return "S";
    case "":
      return "E";
    case "héllo":
      return "H";
    default:
      return "?";
  }
}

function same(a: unknown): string {
  return [a === "abc", a !== "abc", "abc" === a, a === "", a === "€"].map((b) => (b ? 1 : 0)).join("");
}

function differs(xs: unknown[]): string {
  return xs.map((x) => same(x) + kind(x)).join(" ");
}

function ropes(n: number): string {
  // Long concatenations make ropes; slices make slice strings.
  let s = "";
  for (let i = 0; i < n; i++) s += "ab";
  const long = s + "c";
  const sl = ("xxuserxx" as string).slice(2, 6);
  const parts = "user path status héllo".split(" ");
  return [long === s + "c", long.slice(-3) === "abc", kind(sl), parts.map(kind).join(""), kind("us" + "er")].join(",");
}

console.log(differs(["abc", "ab" + "c", "abd", "", "€", "e", 1, null, undefined, {}, ["abc"], "abcd"]));
console.log(differs([new String("abc"), "user", "path", "status", "héllo", "hello", "use", "users", 0]));
console.log(ropes(10), ropes(1000));
