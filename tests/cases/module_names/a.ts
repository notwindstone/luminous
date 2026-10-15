export class Thing {
  kind = "a";
}
export function make(): Thing {
  return new Thing();
}
export const arrow = () => "a";
export const Anon = class {};
export const named = function inner() {};
