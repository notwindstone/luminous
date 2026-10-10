// @native isEven isOdd viaInferred inferred usesBad usesUsesBad recA
// @interpreted bad recB
function isEven(n: number): boolean { return n === 0 ? true : isOdd(n - 1); }
function isOdd(n: number): boolean { return n === 0 ? false : isEven(n - 1); }

function bad(n: number): number { debugger; return String(n).length; }
// Calls to an interpreted function go through its binding.
function usesBad(n: number): number { return bad(n) + 1; }
function usesUsesBad(n: number): number { return usesBad(n) * 2; }

function inferred(a: number) { return a * 2; }
function viaInferred(a: number): number { return inferred(a) + 1; }

// Mutual recursion without return annotations: the inner call cannot be
// typed (like tsc's noImplicitAny), so that function stays interpreted.
function recA(n: number) { return n <= 0 ? 0 : recB(n - 1); }
function recB(n: number) { return n <= 0 ? 0 : recA(n - 1); }

console.log(isEven(10), isOdd(7), isEven(7), usesUsesBad(12345), viaInferred(4), recA(5));
