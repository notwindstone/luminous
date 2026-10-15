import { greet } from "demo:greet";
// A global, declared (whichever module has a binding of that name).
declare const fromPrelude: string;
console.log(fromPrelude);
console.log(greet("beyond"));
// A global that `--global` makes demo:greet's binding (the same class).
const g = new Greeter("hi");
console.log(g.greet("there"), Greeter === (globalThis as any).Greeter);
await Promise.resolve();
console.log("after await");
