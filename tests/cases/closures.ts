// @native counter adders debouncedFn compose memo outer tdzClosure recurse sharedLoop paramCapture
import { setTimeout, clearTimeout } from "qjs:os";
aot function counter(start: number): () => number {
  let n = start;
  return () => ++n;
}
aot function adders(): Array<() => number> {
  const fns: Array<() => number> = [];
  // Each iteration gets its own `i`.
  for (let i = 0; i < 3; i++) fns.push(() => i * 10);
  // A `var` is shared by all closures.
  for (var j = 0; j < 3; j++) fns.push(() => j);
  return fns;
}
aot function debouncedFn(delay: number, callback: () => void): () => void {
  let timeoutId: number;

  return aot (): void => {
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => callback(), delay);
  };
}
aot function compose(f: (x: number) => number, g: (x: number) => number) {
  return (x: number) => f(g(x));
}
aot function memo(f: (n: number) => number) {
  const cache: Record<number, number> = {};
  let hits = 0;
  return {
    get: (n: number) => {
      if (n in cache) {
        hits++;
        return cache[n];
      }
      return (cache[n] = f(n));
    },
    hits: () => hits,
  };
}
aot function outer(a: number) {
  // Three levels: the innermost closure captures `a` through `middle`.
  function middle(b: number) {
    return (c: number) => a * 100 + b * 10 + c;
  }
  return middle(2)(3);
}
aot function tdzClosure(): string {
  const read = () => late;
  let out = "";
  try {
    out += read();
  } catch (e) {
    out += (e as Error).name + ": " + (e as Error).message;
  }
  let late = "now set";
  return out + " / " + read();
}
aot function recurse(n: number): number {
  // A nested function declaration, hoisted and recursive.
  return fact(n);
  function fact(k: number): number {
    return k <= 1 ? 1 : k * fact(k - 1);
  }
}
aot function sharedLoop(): string {
  let total = 0;
  const add = (x: number) => { total += x; };
  for (let i = 1; i <= 4; i++) add(i);
  return "total=" + total;
}
aot function paramCapture(x: number) {
  const get = () => x;
  x = x * 2;
  return get();
}

const c = counter(5);
console.log(c(), c(), counter(0)(), c());
console.log(adders().map((f) => f()).join(","));
console.log(compose((x) => x + 1, (x) => x * 2)(5), outer(1), recurse(6), sharedLoop(), paramCapture(21));
const m = memo((n) => n * n);
console.log(m.get(4), m.get(4), m.get(5), m.hits());
console.log(tdzClosure());
console.log(typeof c, c.name, c.length, compose.name, debouncedFn.length);

let calls = 0;
const d = debouncedFn(10, () => {
  calls++;
  console.log("debounced call", calls);
});
d();
d();
d();
setTimeout(() => console.log("calls after 50ms:", calls), 50);
