// @native report hypot tiny
// Packages from `node_modules` (the nearest one up from the importing
// module): a scoped package's `exports` (the `import` condition, a
// subpath pattern), a package's `module` field, and a package's
// `index.js`; their JavaScript is compiled with the program. A package
// that says it has no side effects (`"sideEffects"`) brings only the
// modules the program uses (and those it says have side effects).
import { report } from "./module_packages/app";
import { pure } from "./module_packages/pure";
console.log(report());
console.log(pure());
