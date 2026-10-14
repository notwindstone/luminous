// @native slices substrings searches others failing
// @interpreted patchSlice restoreSlice
// String methods with a direct entry (slice, substring, indexOf, includes,
// startsWith, endsWith) on strings: int32 bounds, other numbers, values
// converted by valueOf, missing arguments, ropes; other receivers and a
// replaced method are called as usual.

let conversions = 0;
const three = {
  valueOf(): number {
    conversions++;
    return 3;
  },
};

function slices(s: string): string {
  const n = 2;
  return [
    s.slice(1, 4),
    s.slice(-3),
    s.slice(2),
    s.slice(),
    s.slice(4, 1),
    s.slice(-100, 100),
    s.slice(1.7, 3.2),
    s.slice(NaN, 2),
    s.slice(0, -1),
    s.slice(three as unknown as number),
    s.slice(n, undefined),
    s.slice("1" as unknown as number, "3" as unknown as number),
  ].join("|");
}

function substrings(s: string): string {
  return [
    s.substring(1, 4),
    s.substring(4, 1),
    s.substring(-3),
    s.substring(2),
    s.substring(),
    s.substring(1.5, 100),
    s.substring(NaN, 2),
    s.substring(three as unknown as number, 0),
  ].join("|");
}

function searches(s: string, w: string): string {
  return [
    s.indexOf(w),
    s.indexOf(w, 3),
    s.indexOf(w, -5),
    s.indexOf(w, 1000),
    s.indexOf(""),
    s.indexOf("", 1000),
    s.indexOf(undefined as unknown as string),
    s.indexOf(5 as unknown as string),
    s.includes(w),
    s.includes(w, 10),
    s.startsWith(w),
    s.startsWith(w, s.indexOf(w)),
    s.endsWith(w),
    s.endsWith("o", 5),
    s.endsWith(w, three as unknown as number),
  ].join(",");
}

function others(): string {
  // Arrays have methods of these names too.
  const xs = [1, 2, 3, 2, 1];
  const ys = xs.slice(1, -1);
  return [ys.join(""), xs.indexOf(2), xs.indexOf(2, 2), xs.includes(3), xs.slice(-2).length].join(" ");
}

function failing(s: string, re: unknown): string {
  const out: string[] = [];
  try {
    s.includes(re as string);
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  try {
    s.startsWith(Symbol("x") as unknown as string);
  } catch (e) {
    out.push((e as Error).constructor.name);
  }
  try {
    s.slice({
      valueOf(): number {
        throw new RangeError("no");
      },
    } as unknown as number);
  } catch (e) {
    out.push((e as Error).message);
  }
  return out.join(" ");
}

let savedSlice: unknown;
function patchSlice(): void {
  "use no aot";
  savedSlice = String.prototype.slice;
  String.prototype.slice = function (this: string, a?: number) {
    return "<" + this + ":" + a + ">";
  };
}

function restoreSlice(): void {
  "use no aot";
  String.prototype.slice = savedSlice as typeof String.prototype.slice;
}

const text = "hello world, hello again";
console.log(slices(text));
console.log(substrings(text));
console.log(searches(text, "hello"), searches(text, "xyz"));
console.log(slices("δέλτα ωμέγα"), searches("δέλτα ωμέγα", "ω"));
let rope = "";
for (let i = 0; i < 30; i++) rope = rope + i + ",";
console.log(slices(rope), searches(rope, "17,"));
console.log(others(), failing(text, new RegExp("x")), conversions);
patchSlice();
console.log(slices("abc"));
restoreSlice();
console.log(slices("abcdef"));
