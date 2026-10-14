// @native mathOfValues order remainders signs counters fieldUpdates loopRems
// @interpreted bigints
// `Math` functions on JS values convert them with ToNumber once all the
// arguments are evaluated; integer `%` with exact operands; integer locals
// derived from others; compound assignments to fields of `aot type` array
// elements.

aot type B = { x: number; n: int };

function errorOf(f: () => unknown): string {
  try {
    return String(f());
  } catch (e) {
    return (e as Error).constructor.name + ": " + (e as Error).message;
  }
}

function mathOfValues(): string {
  const vals: unknown[] = [4, -2.5, "9", " 16 ", "", "x", null, undefined, true, false, [25], [], {}, -0];
  const out: string[] = [];
  for (const v of vals) {
    const a = v as number;
    out.push(
      [Math.sqrt(a), Math.abs(a), Math.floor(a), Math.round(a), Math.sign(a), Math.clz32(a), Math.max(a, 1)]
        .map((x) => (Object.is(x, -0) ? "-0" : String(x)))
        .join(" "),
    );
  }
  return out.join(" | ");
}

function order(): string {
  const log: string[] = [];
  const mk = (name: string, v: number) => ({
    valueOf() {
      log.push(name);
      return v;
    },
  });
  const pick = (name: string, v: unknown) => {
    log.push("eval " + name);
    return v;
  };
  const a = pick("a", mk("a", 3)) as unknown as number;
  const r1 = Math.atan2(pick("y", mk("y", 1)) as number, pick("x", mk("x", 1)) as number);
  const r2 = Math.pow(pick("b", mk("b", 2)) as number, pick("e", mk("e", 10)) as number);
  const r3 = Math.min(pick("p", mk("p", 7)) as number, pick("q", mk("q", 5)) as number);
  const r4 = Math.hypot(a, pick("h", mk("h", 4)) as number);
  const r5 = Math.imul(pick("i", mk("i", 0x7fffffff)) as number, pick("j", mk("j", 3)) as number);
  return [r1, r2, r3, r4, r5].join(",") + " / " + log.join(",");
}

function bigints(): string {
  const big = 10n as unknown as number;
  return [errorOf(() => Math.sqrt(big)), errorOf(() => Math.max(1, big)), errorOf(() => Math.abs(big))].join(" | ");
}

function remainders(): string {
  const out: string[] = [];
  let x = 2463534242 | 0;
  let acc = 0;
  for (let i = 0; i < 1000; i++) {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    acc = (acc + ((x >>> 0) % 1000)) | 0;
    if (i % 250 === 0) out.push(String((x >>> 0) % 7), String(x % 7), String((x >>> 3) % -9));
  }
  out.push(String(acc));
  // Int locals: exact remainders, the dividend's sign, NaN for % 0.
  for (let n = -6; n <= 6; n += 3) {
    for (let d = -4; d <= 4; d += 2) {
      const r = n % d;
      out.push(Object.is(r, -0) ? "-0" : String(r));
    }
  }
  let big = 9007199254740000;
  for (let k = 0; k < 3; k++) big += 250;
  out.push(String(big % 1000), String(big % 7), String(-big % 1000));
  return out.join(",");
}

function signs(n: number): string {
  // A double dividend and an int divisor.
  const out: string[] = [];
  for (let d = 1; d <= 5; d++) {
    const r = n % d;
    out.push(Object.is(r, -0) ? "-0" : String(r));
  }
  return out.join(",");
}

function counters(): string {
  const out: number[] = [];
  for (let i = 0; i < 4; i++) {
    for (let j = i + 1; j < 5; j++) out.push(i * 10 + j);
    let k = i - 3;
    k += 2;
    out.push(k);
  }
  // Past 2^53, a derived counter rounds as doubles do.
  let a = 9007199254740990;
  for (let s = 0; s < 4; s++) {
    const b = a + 3;
    out.push(b - 9007199254740000);
    a = b - 1;
  }
  return out.join(",");
}

function fieldUpdates(): string {
  const bs: B[] = [
    { x: 1.5, n: 1 },
    { x: 2.5, n: 2 },
  ];
  for (let i = 0; i < bs.length; i++) {
    bs[i].x -= 0.25;
    bs[i].n += 2147483647;
    bs[i].x *= 2;
    bs[i].n++;
    const b = bs[i];
    b.x += i;
    b.n -= 1;
  }
  const holes: B[] = new Array(1);
  const errors = [
    errorOf(() => {
      holes[0].x += 1;
    }),
    errorOf(() => {
      holes[0].n++;
    }),
  ];
  return JSON.stringify(bs) + " " + errors.join(" | ");
}

function loopRems(n: number): string {
  // Loop counters bounded below divide without checks; remainders compare
  // as integers on the fast path and as doubles otherwise.
  const out: (number | string)[] = [];
  let hits = 0;
  for (let d = 3; d * d <= Math.abs(n) + 50; d += 2) {
    const r = n % d;
    if (r === 0) hits++;
    if (r < 1) out.push(Object.is(r, -0) ? "-0" : r);
    if (n % d !== 2) hits += 10;
  }
  for (let k = 1; k < 6; k++) {
    out.push(-k % k === 0 ? "z" : "nz", Object.is(-k % k, -0) ? "-0" : "+", k % 4 < 2 ? "lt" : "ge");
  }
  let m = 0;
  for (let k = 0; k < 4; k++) m += 7 % (k + 1);
  out.push(hits, m);
  return out.join(",");
}

console.log(mathOfValues());
console.log(order());
console.log(bigints());
console.log(remainders());
console.log(signs(7.5), signs(-8), signs(-0), signs(1e300), signs(NaN));
console.log(counters());
console.log(fieldUpdates());
console.log(loopRems(-96), loopRems(97), loopRems(10.5), loopRems(-0), loopRems(NaN), loopRems(1e17));
