// @error `aot` function `top` cannot be compiled natively
// @error `helper` is interpreted: unsupported statement
// @error `mid` is interpreted: calls `helper`
function helper(x: number): number { debugger; return x; }
function mid(x: number): number { return helper(x) + 1; }
aot function top(x: number): number { return mid(x) * 2; }
