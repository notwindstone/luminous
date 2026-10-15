export class Thing {
  kind = "b";
}
export function make(): Thing {
  return new Thing();
}
export const arrow = () => "b";
export const Anon = class {};
export const named = function inner() {};
