import { value as eagerValue, Base } from "./eager.ts";
import { helper } from "./helper.ts";
console.log("lazy evaluating");
export const value = helper(eagerValue);
export function twice(x: number): number {
  return 2 * x;
}
export let counter = 0;
export function bump(): void {
  counter++;
}
class Local {
  name(): string {
    return "local";
  }
}
class Derived extends Local {
  name(): string {
    return "derived:" + super.name();
  }
}
class FromEager extends Base {}
export const named = (): string => new Derived().name() + "/" + new FromEager().name();
export default [1, 2, 3].map((x) => x * value).join("+");
