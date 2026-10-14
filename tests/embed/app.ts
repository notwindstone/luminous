import { greet } from "demo:greet";
console.log((globalThis as any).fromPrelude);
console.log(greet("beyond"));
await Promise.resolve();
console.log("after await");
