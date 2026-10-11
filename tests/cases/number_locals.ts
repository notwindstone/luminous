// @native nullish retyped hoisted mixed zeros fields int32s
function nullish(): string {
  const vals: unknown[] = [undefined, null, 0, "", false, NaN, {}, [], "null"];
  return vals.map((v) => `${v == null}${v != undefined}${undefined == v}${null != v}`).join(",");
}

function retyped(n: number): string {
  // Untyped because of the object field; only ever numbers.
  const p = { x: 0.5 };
  let acc = 0;
  let prod = 1;
  for (let i = 0; i < n; i++) {
    p.x = p.x * 1.5;
    acc += p.x * 2 - 1;
    prod = prod * (i + 1);
  }
  return [acc, prod, typeof acc, acc === acc].join(",");
}

function hoisted(): string {
  const before = typeof late;
  var late = 1;
  late = late * 2;
  return before + "," + late;
}

function mixed(): string {
  const o = { v: 1 };
  let a = o.v + 1;
  a = a * 3;
  let s = o.v + 1;
  s = s + "x";
  let t = o.v * 2;
  if (t > 1) t = t - 0.5;
  return [a, s, t, typeof s].join(",");
}

function zeros(): string {
  const o = { z: 0 };
  let neg = o.z * -1;
  const nan = o.z / o.z;
  let inf = 1 / neg;
  inf = inf * 2;
  return [Object.is(neg, -0), Object.is(nan, NaN), inf, nan === nan].join(",");
}

function fields(): string {
  const p = { a: 1, b: 2 };
  p.a = p.a + p.b;
  p.b = p.a * p.b;
  const q = { s: 1 };
  q.s = q.s + "!";
  return p.a + "," + p.b + "," + q.s;
}

function int32s(seed: number): string {
  let x = seed | 0;
  let acc = 0x7ffffff0 | 0;
  let wide = 0;
  for (let i = 0; i < 40; i++) {
    x ^= x << 13;
    x ^= x >>> 17;
    x ^= x << 5;
    acc = (acc + x) | 0;
    wide = acc + x;
  }
  const big = (0x7fffffff | 0) + (0x7fffffff | 0);
  return [x, acc, wide, big, (acc ^ x) >>> 0].join(",");
}

console.log(nullish());
console.log(retyped(10), retyped(0));
console.log(hoisted());
console.log(mixed());
console.log(zeros());
console.log(fields());
console.log(int32s(12345), int32s(-1), int32s(0x9e3779b9));
