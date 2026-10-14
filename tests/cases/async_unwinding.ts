// An async function that ends with an exception from a native callee must
// not leave the exception pending for native code that runs next.
function evaluate(): void {
  throw "foo";
}
let stateA = 0;
let stateB = 0;
let errorB: unknown;
function initA(): void {
  if (stateA) return;
  stateA = 1;
}
function initB(): void {
  if (stateB) {
    if (stateB === 3) throw errorB;
    return;
  }
  stateB = 1;
  try {
    evaluate();
  } catch (e) {
    stateB = 3;
    errorB = e;
    throw e;
  }
  stateB = 2;
}
let cached: { x: number } | undefined;
function namespaceA(): { x: number } {
  return cached || (cached = new Proxy({ x: 42 }, { get: (t, k) => Reflect.get(t, k) }));
}
function loadA(): Promise<{ x: number }> {
  return (async () => {
    await null;
    await initA();
    return namespaceA();
  })();
}
function loadB(): Promise<void> {
  return (async () => {
    await null;
    await initB();
  })();
}
async function main(): Promise<void> {
  const a = loadA();
  const b = loadB();
  console.log("a:", (await a).x);
  try {
    await b;
  } catch (e) {
    console.log("b:", e);
  }
}
main().then(
  () => console.log("done"),
  (e) => console.log("main:", e),
);
