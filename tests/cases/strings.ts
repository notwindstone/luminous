// @native greet describe concat compare shout pad codes
aot function greet(name: string): string {
  return "Hello, " + name + "!";
}
aot function describe(x: unknown): string {
  return `value=${x} type=${typeof x}`;
}
aot function concat(a: unknown, b: unknown): unknown {
  return a + b;
}
aot function compare(a: string, b: string): string {
  if (a < b) return "lt";
  if (a === b) return "eq";
  return "gt";
}
aot function shout(s: string): string {
  return s.toUpperCase().split("").join("-") + " (" + s.length + ")";
}
aot function pad(n: number, width: number): string {
  let s = "" + n;
  while (s.length < width) s = "0" + s;
  return s;
}
aot function codes(s: string): number {
  let sum = 0;
  for (let i = 0; i < s.length; i++) sum += s.charCodeAt(i) * (i + 1);
  return sum;
}

console.log(greet("world"), greet(""), greet("ünïcødé ✓"));
for (const x of [1, 1.5, -0, NaN, true, null, undefined, "s", [1, 2], { a: 1 }]) console.log(describe(x));
console.log(concat(1, 2), concat("1", 2), concat(1, "2"), concat([1], [2]), concat({}, "x"), concat(null, 1));
console.log(concat(1n, 2n), concat(undefined, "!"), concat({ valueOf: () => 40 }, 2), concat({ toString: () => "T" }, "x"));
console.log(compare("a", "b"), compare("b", "a"), compare("same", "same"), compare("10", "9"));
console.log(shout("abc"), pad(7, 3), pad(12345, 3), codes("hello"));
try {
  describe(Symbol("s"));
} catch (e) {
  console.log((e as Error).name);
}
