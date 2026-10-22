// @native mix step2 step3 step4 chain rare guarded deep leafy t1 t2 t3 t4 t5 t6 t7 t8 t9 t10 nine
// Calls of functions that cannot throw (effect summaries) skip the test
// for an exception after them, and such functions with a bounded call
// tree make no stack check of their own: exceptions still surface where
// they would, a callee that throws only on rare input included, and a
// recursion still overflows the stack with the engine's RangeError.
function mix(a: number, b: number): number {
  let x = a;
  for (let i = 0; i < 3; i++) x = (x * 31 + b) % 65521;
  return x;
}
function step2(a: number): number {
  return mix(a, 7) + mix(a, 11);
}
function step3(a: number): number {
  let s = 0;
  for (let i = 0; i < 2; i++) s += step2(a + i);
  return s;
}
function step4(a: number): number {
  return step3(a) - step2(a);
}
function chain(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) total = (total + step4(i)) % 1000003;
  return total;
}

function rare(x: number): number {
  if (x === 777) throw new RangeError("rare input " + x);
  return x * 2;
}
function guarded(n: number): string {
  let sum = 0;
  let caught = "";
  for (let i = 0; i < n; i++) {
    try {
      sum += rare(i) + mix(i, 3);
    } catch (e: any) {
      caught += e.message + ";";
    }
  }
  return sum + " " + caught;
}

function leafy(n: number): number {
  return (n * 3) % 7;
}
function deep(n: number): number {
  return n <= 0 ? 0 : leafy(n) + deep(n - 1);
}

// Ten nested callees that cannot throw: the outer ones exceed the depth
// within which no stack check is made.
function t1(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = x + 1;
  return r;
}
function t2(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t1(x) * 2;
  return r;
}
function t3(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t2(x) + 3;
  return r;
}
function t4(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t3(x) * 3;
  return r;
}
function t5(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t4(x) - 5;
  return r;
}
function t6(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t5(x) + 6;
  return r;
}
function t7(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t6(x) % 1000;
  return r;
}
function t8(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t7(x) + 8;
  return r;
}
function t9(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t8(x) * 9;
  return r;
}
function t10(x: number): number {
  let r = 0;
  for (let k = 0; k < 1; k++) r = t9(x) - 10;
  return r;
}
function nine(n: number): number {
  let s = 0;
  for (let i = 0; i < n; i++) s += t10(i) + t5(i);
  return s;
}

console.log(chain(2000));
console.log(guarded(1000));
console.log(deep(1000));
try {
  console.log(deep(10_000_000));
} catch (e: any) {
  console.log(e instanceof RangeError, e.message);
}
console.log(nine(500));
