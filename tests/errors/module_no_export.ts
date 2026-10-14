// @error module_no_export.ts:3: module
// @error has no export `nope`
import { nope } from "./modules/badmod";
console.log(nope);
