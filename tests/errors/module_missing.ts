// @error module_missing.ts:3: cannot find module `./modules/nothere`
import { ok } from "./modules/badmod";
import { x } from "./modules/nothere";
console.log(ok(), x);
