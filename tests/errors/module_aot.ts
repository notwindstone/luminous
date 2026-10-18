// @error `aot` function `f` cannot be compiled natively
// @error unsupported expression: new.target
// @error modules/badmod.ts:5
import { f } from "./modules/badmod";
console.log(f(1));
