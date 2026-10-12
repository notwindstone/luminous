// @native lines widths conversions throwing longOnes single
// Template literals are concatenated by one call that copies each part
// once: many parts, empty ones, 16-bit characters, numbers and other values
// converted with toString (not valueOf), a part that throws midway (the
// parts before it are released), results long enough for ropes, and
// templates with a single part.

function lines(n: number): string {
  const out: string[] = [];
  for (let i = 0; i < n; i++) {
    const sec = String(i % 60).padStart(2, "0");
    out.push(`2024-03-15T10:${String((i / 60) % 60 | 0).padStart(2, "0")}:${sec}Z ${i % 3 === 0 ? "INFO" : "WARN"} [svc${i % 7}] ` + `user=u${i} bytes=${(i * 7919) % 50000}`);
  }
  return out.join("\n");
}

function widths(): string {
  const a = "λ";
  const b = "plain";
  const e = "";
  return [`${a}${b}${e}`, `${e}${e}${b}`, `x${a}y${a}z`, `${b}-${b}-${b}`, `${e}`, `${e}${e}`, `é${b}ü`].join("|");
}

class Money {
  cents: number;
  constructor(cents: number) {
    this.cents = cents;
  }
  toString(): string {
    return `$${(this.cents / 100).toFixed(2)}`;
  }
  valueOf(): number {
    return this.cents;
  }
}

function conversions(): string {
  const m = new Money(1234);
  const o = { toString: () => "obj" };
  const arr = [1, [2, 3], "x"];
  return `${m} ${m + ""} ${o} ${arr} ${null} ${undefined} ${true} ${-0} ${1e21} ${0.1 + 0.2} ${NaN}`;
}

let made = 0;
function part(s: string): string {
  made++;
  return s.repeat(3);
}

function throwing(): string {
  const bad = {
    toString(): string {
      throw new Error("no string");
    },
  };
  try {
    return `${part("a")}${part("b")}${bad}${part("c")}`;
  } catch (e) {
    let symbolMsg = "";
    try {
      const s = Symbol("s");
      symbolMsg = `${part("d")}${s as unknown as string}`;
    } catch (e2) {
      symbolMsg = (e2 as Error).constructor.name;
    }
    return (e as Error).message + " " + made + " " + symbolMsg;
  }
}

function longOnes(): string {
  const big = "0123456789".repeat(500);
  let rope = "";
  for (let i = 0; i < 40; i++) rope += big.slice(i, i + 300);
  const t1 = `${big}|${big}`;
  const t2 = `<${rope}>${rope.length}`;
  const t3 = `${t1}${t2}${t1}`;
  return t1.length + " " + t2.length + " " + t3.length + " " + t3.slice(4995, 5010) + " " + t3.slice(-20) + " " + (t2.charCodeAt(5000) | 0);
}

function single(x: unknown): string {
  return `${x}`;
}

console.log(lines(5));
console.log(widths());
console.log(conversions());
console.log(throwing());
console.log(longOnes());
console.log(single(42), single("s"), single(null), single({}), single([1, 2]));
