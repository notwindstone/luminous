// @interpreted f
function f(x: number): number { return x * 2; }
eval("f = function (x) { return -x; }");
console.log(f(21));
