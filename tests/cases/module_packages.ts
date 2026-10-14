// @native report hypot tiny
// Packages from `node_modules` (the nearest one up from the importing
// module): a scoped package's `exports` (the `import` condition, a
// subpath pattern), a package's `module` field, and a package's
// `index.js`; their JavaScript is compiled with the program.
import { report } from "./module_packages/app";
console.log(report());
