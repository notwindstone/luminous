// @error uses_cjs.ts:1: `
// @error node_modules/cjs-lib/index.js` is a CommonJS module (`module.exports`, `require`), which is not supported yet
import { hi } from "./modules/uses_cjs";
console.log(hi);
