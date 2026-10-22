// Loops that call small helpers which are not inlined (each has a loop)
// and read fields, with configuration read in the loops: what effect
// summaries of the callees let the caller keep across the calls.
class Account {
  constructor(
    public balance: number,
    public rate: number,
    public limit: number,
  ) {}
  // Read-only helper: reads fields, writes nothing.
  interest(months: number): number {
    let b = this.balance;
    for (let m = 0; m < months; m++) b += b * this.rate;
    return b - this.balance;
  }
}

type Config = { scale: number; offset: number; cap: number; steps: number };
const config: Config = { scale: 1.5, offset: 3, cap: 1e9, steps: 3 };

// A helper that reads its arguments and nothing else.
function weigh(x: number, steps: number): number {
  let w = x;
  for (let k = 0; k < steps; k++) w = (w * 7 + k) % 1009;
  return w;
}

// Config fields read in the loop, around calls that write nothing.
function scaled(n: number): number {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const w = weigh(i, config.steps);
    total += w * config.scale + config.offset;
    if (total > config.cap) total = 0;
  }
  return total;
}

// Fields of an object read before and after calls that do not write them.
function accrue(accounts: Account[], rounds: number): number {
  let sum = 0;
  for (let r = 0; r < rounds; r++) {
    for (let i = 0; i < accounts.length; i++) {
      const a = accounts[i];
      const before = a.balance;
      const gain = a.interest(2);
      if (a.balance + gain < a.limit) sum += gain + before * 0.001;
    }
  }
  return sum;
}

// A helper too large to inline that reads only its arguments, called in a
// test and again in its branch; fields of parsed objects read again after
// such calls.
function rank(price: number, qty: number): number {
  let r = 0;
  for (let k = 1; k <= 3; k++) {
    const t = (price * k + qty) % 11;
    r += t > 5 ? t - 5 : t * 0.5;
    if (r > 50) r = r / 2;
  }
  return r + (price > qty ? 1 : 0) + (qty === 0 ? 2 : 0);
}
function settle(orders: any[], rounds: number): number {
  let total = 0;
  for (let r = 0; r < rounds; r++) {
    for (let i = 0; i < orders.length; i++) {
      const o = orders[i];
      const p = o.price;
      const q = o.qty;
      if (rank(p, q) > 3) total += rank(p, q) + o.price * 0.01;
      else total += o.price * q * 0.001;
    }
  }
  return total;
}

const orders: any[] = JSON.parse(
  JSON.stringify(Array.from({ length: 1000 }, (_, i) => ({ id: i, price: 10 + (i % 37), qty: i % 13 }))),
);

const accounts: Account[] = [];
for (let i = 0; i < 1000; i++) accounts.push(new Account(100 + i, 0.001 * (i % 7), 1e6));

const start = Date.now();
let s = 0;
for (let round = 0; round < 10; round++) s += scaled(1_000_000);
const mid = Date.now();
const a = accrue(accounts, 3000);
const mid2 = Date.now();
const t = settle(orders, 3000);
const end = Date.now();
console.log(`scaled: ${s.toFixed(3)}`);
console.log(`accrue: ${a.toFixed(3)}`);
console.log(`settle: ${t.toFixed(3)}`);
console.log(`elapsed scaled: ${mid - start} ms`);
console.log(`elapsed accrue: ${mid2 - mid} ms`);
console.log(`elapsed settle: ${end - mid2} ms`);
console.log(`elapsed: ${end - start} ms`);
