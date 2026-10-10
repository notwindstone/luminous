// @error `aot` function `top` cannot be compiled natively
// @error `helper` is interpreted: property access is not supported
// @error `mid` is interpreted: calls `helper`
function helper(x: number): number { return ("" + x).length; }
function mid(x: number): number { return helper(x) + 1; }
aot function top(x: number): number { return mid(x) * 2; }
