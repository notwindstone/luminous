import { used, util } from "pure-lib";
// `Shape` and `Options` are types of the package (in its .d.ts), not of its
// JavaScript: they are erased.
import type { Options } from "pure-lib";

type Shape = util.Shape;

export function pure(): string {
  // A local named as the binding `util.isNumber` stands for.
  const isNumber = util.isNumber;
  const opts: Options | undefined = undefined;
  const s: Shape | undefined = undefined;
  return [used(21), isNumber(1), isNumber("x"), (globalThis as any).pureLibPolyfill, opts, s].join(" ");
}
