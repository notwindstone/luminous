// Bundled modules that declare the same names are renamed apart (Thing,
// Thing$1); functions and classes keep the `name` written in the source.
import * as a from "./module_names/a.ts";
import * as b from "./module_names/b.ts";

class Thing {}
const t = new Thing();
console.log(Thing.name, t.constructor.name);
console.log(a.Thing.name, b.Thing.name, a.make().constructor.name, b.make().kind);
console.log(a.make.name, b.make.name, a.arrow.name, b.arrow.name, b.arrow());
console.log(a.Anon.name, b.Anon.name, a.named.name, b.named.name);
