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
export default (class {
  static make(): string {
    return "made";
  }
});
