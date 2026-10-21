export let count = 0;
export var hoisted = "var";
export const fixed = 1;
export function bump(): number {
  return ++count;
}
export function self(this: unknown): unknown {
  return this;
}
export function plain(): string {
  return "plain";
}
// (Bound functions ignore the namespace; a variable assigned again might
// not.)
export const bound = self.bind("bound");
export const picked = count > 100 ? self.bind("big") : self.bind("small");
export let later = (): unknown => "arrow";
export function relink(): void {
  later = self;
}
export default (class {
  static make(): string {
    return "made";
  }
});
