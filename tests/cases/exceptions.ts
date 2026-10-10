// @native thrower safeDiv tryFinally loopFinally nested rethrow fromInterp deep catchOverflow returnInFinally
const log: string[] = [];
aot function thrower(msg: string): never {
  throw new Error(msg);
}
aot function safeDiv(a: number, b: number): unknown {
  try {
    if (b === 0) throw new RangeError("division by zero");
    return a / b;
  } catch (e) {
    return "caught " + (e as Error).name + ": " + (e as Error).message;
  }
}
aot function tryFinally(n: number): string {
  let s = "";
  try {
    s += "try;";
    if (n > 0) throw "boom";
    s += "after;";
  } catch (e) {
    s += "catch " + e + ";";
  } finally {
    s += "finally;";
  }
  return s;
}
aot function loopFinally(): string {
  let s = "";
  for (let i = 0; i < 5; i++) {
    try {
      if (i === 1) continue;
      if (i === 3) break;
      s += "i" + i + ";";
    } finally {
      s += "f" + i + ";";
    }
  }
  return s;
}
aot function nested(): string {
  let s = "";
  try {
    try {
      throw new TypeError("inner");
    } finally {
      s += "inner-finally;";
    }
  } catch (e) {
    s += "outer-catch " + (e as Error).message + ";";
  }
  return s;
}
aot function rethrow(): void {
  try {
    thrower("first");
  } catch (e) {
    throw new Error("wrapped: " + (e as Error).message);
  }
}
aot function fromInterp(f: () => void): string {
  try {
    f();
    return "no throw";
  } catch (e) {
    return "native caught " + e;
  }
}
aot function deep(n: number): number {
  if (n === 0) throw new Error("bottom");
  return deep(n - 1) + 1;
}
aot function catchOverflow(): string {
  try {
    return "" + deep(1e9);
  } catch (e) {
    return (e as Error).name;
  }
}
aot function returnInFinally(): string {
  let s = "";
  try {
    s += "a";
    return s + "-returned";
  } finally {
    log.push("finally ran with " + s);
  }
}

for (const f of [() => thrower("x"), () => rethrow(), () => deep(50)]) {
  try {
    f();
  } catch (e) {
    console.log((e as Error).name, (e as Error).message);
  }
}
console.log(safeDiv(1, 2), safeDiv(1, 0));
console.log(tryFinally(0), tryFinally(1), loopFinally(), nested());
console.log(fromInterp(() => {}), fromInterp(() => { throw "interp error"; }));
console.log(catchOverflow(), returnInFinally(), log.join("|"));
