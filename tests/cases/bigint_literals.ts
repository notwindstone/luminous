// @native literals arith compare main
// @interpreted huge
// BigInt literals below 2^64 compile (made by JS_NewBigUint64); larger ones
// keep their function interpreted.
function literals(): string {
  const xs = [0n, 1n, -1n, 42n, 0x7fffffffn, 0x80000000n, 0xffffffffn, 0x100000000n, 9007199254740993n, 9223372036854775807n, 18446744073709551615n, 0b1010n, 0o777n, 1_000_000n];
  return xs.map((x) => typeof x + ":" + x.toString()).join(" ");
}
function arith(n: bigint): string {
  const max = 9223372036854775807n - BigInt(n);
  return [max, max + 1n, -max - 1n, n * 2n, n / 3n, n % 7n, n ** 3n, -n, n << 40n].join(",");
}
function compare(x: any): string {
  return [x === 0n, x == 0, x < 10n, x > -1n, x === 42n, 42n === x, typeof x === "bigint"].join(",");
}
function huge(): bigint {
  return 123456789012345678901234567890n;
}
function main() {
  console.log(literals());
  console.log(arith(5n), arith(-123456789n));
  for (const x of [0n, 42n, 0, -5n, huge(), "0"]) console.log(compare(x));
  console.log(String(huge()), String(huge() + 1n));
}
main();
