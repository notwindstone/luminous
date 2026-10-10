// @error `aot` cannot be applied to this arrow function
// @error `aot` cannot be applied to this function
const xs = [1, 2, 3].map(aot (x: number): number => x * 2);
function outer(): number {
  function inner(y: number): number {
    "use aot";
    return y;
  }
  return inner(1);
}
console.log(xs, outer());
