// @native nearLimit negLimit bigSteps mixed indexes bitwise zeros loops countdown modified modifiedDown gen asyncCount
function nearLimit(): string {
  let x = 9007199254740985;
  const seen: number[] = [];
  for (let i = 0; i < 12; i++) {
    x++;
    seen.push(x);
  }
  let y = 9007199254740990;
  y += 3;
  y += 3;
  y += 1;
  return seen.join(",") + "|" + y + "|" + (x === 9007199254740992);
}

function negLimit(): string {
  let x = -9007199254740985;
  for (let i = 0; i < 12; i++) x--;
  let y = -9007199254740990;
  for (let i = 0; i < 4; i++) y -= 3;
  return x + "|" + y;
}

function bigSteps(): string {
  let x = 9007199254000000;
  for (let i = 0; i < 10000; i++) x += 255;
  let y = 4611686018427387904 / 1;
  let z = 0;
  z += 256;
  z -= 255;
  return x + "|" + y + "|" + z;
}

function mixed(n: number): string {
  let i = 0;
  let below = 0;
  while (i < 2.5) {
    below++;
    i++;
  }
  let big = 2147483647;
  big++;
  let neg = -2147483648;
  neg--;
  let k = 7;
  const half = k / 2;
  return [below, i, big, neg, half, k * n, k - n, String(k), `${big}`, k % 4, -k].join(",");
}

function indexes(xs: number[]): number {
  let s = 0;
  for (let i = 0; i < xs.length; i++) s += xs[i] * i;
  let j = -1;
  const out = xs[j];
  j = 100;
  return s + (out === undefined ? 1000 : 0) + (xs[j] === undefined ? 2000 : 0);
}

function bitwise(): string {
  let a = 0;
  let acc = 0;
  for (let i = 0; i < 40; i++) {
    a += 123456789;
    acc ^= a | 0;
    acc += a & 255;
    acc = acc >>> 0;
  }
  let b = 9007199254740991;
  return [acc, a, ~a, a >> 3, b | 0, b & 0xffff, b >>> 0].join(",");
}

function zeros(): string {
  let a = 0;
  a -= 0;
  let b = 1;
  b--;
  let c = -0;
  let d = 5;
  d -= 5;
  return [Object.is(a, -0), Object.is(b, -0), Object.is(c, -0), Object.is(d, -0), 1 / d].join(",");
}

function loops(): string {
  let out = "";
  let i = 0;
  while (i < 10) {
    i++;
    if (i % 3 === 0) continue;
    if (i === 8) break;
    out += i;
  }
  let j = 0;
  do {
    j += 2;
  } while (j < 7);
  let calls = 0;
  const next = (): boolean => ++calls < 4;
  while (next()) out += "n";
  for (let k = 0; k < 0; k++) out += "never";
  let m = 0;
  for (; m <= 5; m++) {}
  return out + "|" + j + "|" + calls + "|" + m;
}

function countdown(): string {
  let out = "";
  for (let i = 5; i >= 0; i--) out += i;
  for (let i = 3; i > -3; i -= 2) out += "," + i;
  return out;
}

function modified(): string {
  let out = "";
  for (let i = 0; i < 20; i++) {
    if (i === 3) i = 9007199254740990;
    out += i + ";";
    if (out.length > 200) break;
  }
  return out;
}

function modifiedDown(): string {
  let out = "";
  for (let i = 0; i > -10; i--) {
    out += i;
    i -= 2;
  }
  return out;
}

function* gen(): Generator<number> {
  let x = 9007199254740988;
  for (let i = 0; i < 6; i++) {
    yield x;
    x += 1;
  }
}

async function asyncCount(): Promise<number> {
  let n = 0;
  for (let i = 0; i < 5; i++) {
    await null;
    n += 2;
  }
  return n;
}

console.log(nearLimit());
console.log(negLimit());
console.log(bigSteps());
console.log(mixed(3), mixed(0.5));
console.log(indexes([1, 2, 3, 4, 5]));
console.log(bitwise());
console.log(zeros());
console.log(loops());
console.log(countdown());
console.log(modified());
console.log(modifiedDown());
console.log([...gen()].join(","));
asyncCount().then((n) => console.log("async", n));
