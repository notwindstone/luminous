// @error `aot` function `f` cannot be compiled natively
// @error `arguments` is not supported
// @error modules/badmod.ts:5
import { f } from "./modules/badmod";
console.log(f(1));
