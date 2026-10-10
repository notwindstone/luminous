// @native inc get reset setName readLate makeConfig hoisted classRef useFn
let count: number = 0;
let name = "start";
const config = { debug: false, level: 3 };
var hoistedVar = 1;

aot function inc(): number {
  count++;
  return count;
}
aot function get(): number {
  return count;
}
aot function reset(v: number): void {
  count = v;
}
aot function setName(s: string): string {
  name = name + "+" + s;
  return name;
}
aot function readLate(): number {
  return late;
}
aot function makeConfig(): string {
  config.level++;
  return JSON.stringify(config);
}
aot function hoisted(): unknown {
  return hoistedVar;
}
aot function classRef(): string {
  return new Later().hello();
}
function helper(x: number) { return x * 3; }
aot function useFn(x: number): unknown {
  return helper(x) + helper.name;
}

// Native and interpreted code see the same binding.
console.log(inc(), inc(), get(), count);
count += 10;
console.log(get(), inc(), count);
reset(100);
console.log(count, ({ count }).count);
[count] = [7];
console.log(get(), setName("a"), name, setName("b"));
// TDZ: reading `late` before its declaration ran throws, as in the interpreter.
try {
  readLate();
} catch (e) {
  console.log((e as Error).name, (e as Error).message);
}
try {
  classRef();
} catch (e) {
  console.log((e as Error).name, (e as Error).message);
}
let late = 5;
class Later { hello() { return "hello from Later"; } }
console.log(readLate(), classRef(), makeConfig(), makeConfig(), hoisted(), useFn(2));
hoistedVar = 9;
console.log(hoisted());
