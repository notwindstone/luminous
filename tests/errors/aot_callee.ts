// @error `aot` function `top` cannot be compiled natively
// @error unsupported statement: debugger
function helper(x: number): number { debugger; return x; }
aot function top(x: number): number { debugger; return helper(x) * 2; }
