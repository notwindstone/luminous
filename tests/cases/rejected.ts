// @native ok noAnnot str obj fallsOff global useVar closure args
// @interpreted reassigned tdz assignConst
function ok(x: number): number { return x; }
function reassigned(x: number): number { return x; }
function noAnnot(x) { return x; }
function str(x: number): string { return "" + x; }
function closure(x: number): number { const f = () => x; return f(); }
function obj(x: number): number { const o = { x }; return o.x; }
function args(x: number): number { return arguments.length + x; }
function fallsOff(x: number): number { if (x > 0) return 1; }
function tdz(x: number): number { const y = z + x; const z = 1; return y; }
function assignConst(x: number): number { const y = 1; y = x; return y; }
let counter = 0;
function global(x: number): number { return counter + x; }
function useVar(x: number): number { var y = x; return y; }

reassigned = (x: number) => x + 1;
console.log(ok(1), reassigned(1), noAnnot(1), str(1), closure(1), obj(1), args(1), fallsOff(1));
try { tdz(1); } catch (e) { console.log((e as Error).name); }
try { assignConst(1); } catch (e) { console.log((e as Error).name); }
console.log(global(1), useVar(2));
