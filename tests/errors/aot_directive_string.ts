// @error `aot` function `greet` cannot be compiled natively
// @error parameter `name` must be annotated `number` or `boolean`
function greet(name: string): string {
  "use aot";
  return "hi " + name;
}
