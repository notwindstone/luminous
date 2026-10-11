// @error `aot` cannot be applied to this arrow function
// @error `aot` cannot be applied to this function
class A {
  f = aot (x: number): number => x * 2;
  g: () => number;
  constructor() {
    this.g = aot function (): number {
      return 1;
    };
  }
}
const a = new A();
console.log(a.f(1), a.g());
