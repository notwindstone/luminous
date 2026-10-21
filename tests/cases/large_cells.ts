// @native strings arrays maps main
// Cells past the largest size class are chunks of their own, kept when
// freed for the next of about their size (their memory not zeroed again):
// what is made in them must not read what an earlier cell left.
function strings(rounds: number): number {
  let total = 0;
  for (let r = 0; r < rounds; r++) {
    const s = String.fromCharCode(97 + (r % 26)).repeat(100000 + r * 7);
    const t = s + "|" + r;
    total += t.length + t.charCodeAt(r) + t.charCodeAt(t.length - 1);
  }
  return total;
}

function arrays(rounds: number): number {
  let total = 0;
  for (let r = 0; r < rounds; r++) {
    const xs: number[] = [];
    for (let i = 0; i < 20000; i++) xs.push(i * r);
    const holes = new Array(30000);
    holes[29999] = r;
    let undef = 0;
    for (let i = 0; i < holes.length; i += 997) if (holes[i] === undefined) undef++;
    total += xs[19999] + xs.length + undef + holes[29999];
  }
  return total;
}

function maps(rounds: number): number {
  let total = 0;
  for (let r = 0; r < rounds; r++) {
    const m = new Map<number, number>();
    for (let i = 0; i < 5000; i++) m.set(i, i + r);
    total += m.size + (m.get(4999) ?? 0) + (m.get(5000) === undefined ? 1 : 0);
  }
  return total;
}

function main(): void {
  console.log("strings", strings(40));
  console.log("arrays", arrays(20));
  console.log("maps", maps(20));
  const big = new Uint8Array(1 << 20);
  let nonzero = 0;
  for (let i = 0; i < big.length; i += 4096) if (big[i] !== 0) nonzero++;
  console.log("zeroed typed array", nonzero === 0);
}

main();
