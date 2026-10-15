import { Greeter } from "demo:greet";
(globalThis as any).fromPrelude = "prelude ran first";
// A global that the build names a binding of demo:greet (`--global`).
(globalThis as any).Greeter = Greeter;
