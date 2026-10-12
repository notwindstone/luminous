// @native pieces sliced wide fromRope freed
// Short pieces of 8-bit strings (up to 32 bytes) are copied inline into a
// cached block, and the last reference to a plain string returns its block
// directly: every length and offset around the word sizes, pieces of plain,
// sliced (over 1 KiB), rope and 16-bit strings, and atom, sliced and plain
// strings released as they go.

const base = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

function pieces(s: string): string {
  let h = 0;
  const out: string[] = [];
  for (let len = 0; len <= 40; len++) {
    for (let start = 0; start + len <= s.length && start < 200; start += 7) {
      const p = s.slice(start, start + len);
      if (p.length !== len) return "bad length " + start + " " + len;
      for (let i = 0; i < len; i++) h = (h * 31 + p.charCodeAt(i)) | 0;
    }
    out.push(s.substring(3, 3 + len));
  }
  return h + " " + out.slice(0, 4).join("|") + " " + out[8] + " " + out[16] + " " + out[17] + " " + out[32] + " " + out[33];
}

function sliced(): string {
  const big = base.repeat(40);
  const sl = big.slice(5, 2000);
  const inner = sl.slice(1100, 1900);
  return pieces(sl) + " / " + pieces(inner) + " " + sl.length + " " + inner.length;
}

function wide(): string {
  const w = "λ" + base + "μ" + base;
  const latin = w.slice(1, 30);
  return pieces(w) + " / " + pieces(latin) + " " + latin + " " + w.slice(60, 70);
}

function fromRope(): string {
  let r = "";
  for (let i = 0; i < 50; i++) r = r + base.slice(i % 10, (i % 10) + 25) + "-";
  return pieces(r) + " " + r.length;
}

function freed(n: number): string {
  const o: Record<string, number> = { alpha: 1, beta: 2, gamma: 3 };
  const names = Object.keys(o);
  let total = 0;
  for (let i = 0; i < n; i++) {
    const k = names[i % 3];
    const s = k + i;
    const t = s.slice(1);
    const long = (k.repeat(300) + i).slice(1);
    const key = Object.keys(o)[i % 3];
    total += t.length + long.length + o[key] + key.length;
  }
  return total + " " + Object.keys(o).join(",") + " " + o.alpha + o.beta + o.gamma;
}

console.log(pieces(base));
console.log(sliced());
console.log(wide());
console.log(fromRope());
console.log(freed(1000));
