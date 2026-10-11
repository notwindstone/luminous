// @native sum deleting adding protoKeys numeric getters otherObj reassigned symbols shapeChange dict
function sum(o: Record<string, number>): number {
  let s = 0;
  for (const k in o) s += o[k];
  return s;
}

function deleting(): string {
  const o: Record<string, number> = { a: 1, b: 2, c: 3, d: 4 };
  let out = "";
  for (const k in o) {
    out += k + "=" + String(o[k]) + ";";
    if (k === "a") delete o.c;
    if (k === "b") delete o.a;
  }
  return out;
}

function adding(): string {
  const o: Record<string, number> = { a: 1, b: 2 };
  let out = "";
  let n = 0;
  for (const k in o) {
    out += k + "=" + String(o[k]) + ";";
    o["x" + n] = n;
    n++;
  }
  return out + "|" + Object.keys(o).join(",");
}

function protoKeys(): string {
  const base: Record<string, number> = { p: 10, q: 20 };
  const o: Record<string, number> = Object.create(base);
  o.a = 1;
  o.q = 2;
  let out = "";
  for (const k in o) out += k + "=" + String(o[k]) + ";";
  const plain: Record<string, number> = { z: 1 };
  Object.defineProperty(Object.prototype, "leak", { value: 5, enumerable: true, configurable: true });
  for (const k in plain) out += k + ";";
  delete (Object.prototype as Record<string, unknown>).leak;
  return out;
}

function numeric(): string {
  const o: Record<string, number> = { b: 1, 10: 2, a: 3, 2: 4, "4294967295": 5, "-1": 6, "01": 7 };
  let out = "";
  for (const k in o) out += k + "=" + String(o[k]) + ";";
  return out;
}

function getters(): string {
  let calls = 0;
  const o: Record<string, number> = { a: 1 };
  Object.defineProperty(o, "b", {
    get: () => {
      calls++;
      return calls * 100;
    },
    enumerable: true,
  });
  o.c = 3;
  let out = "";
  for (const k in o) out += k + "=" + String(o[k]) + ";";
  Object.defineProperty(o, "hidden", { value: 9, enumerable: false });
  for (const k in o) out += k + ";";
  return out + calls;
}

function otherObj(): string {
  const o: Record<string, number> = { a: 1, b: 2, c: 3 };
  const p: Record<string, number> = { a: 10, b: 20, c: 30 };
  const q: Record<string, number> = { c: 100, b: 200 };
  let out = "";
  for (const k in o) out += String(p[k]) + "," + String(q[k]) + ";";
  return out;
}

function reassigned(): string {
  const o: Record<string, number> = { a: 1, b: 2, c: 3 };
  let out = "";
  for (let k in o) {
    out += String(o[k]) + ",";
    k = k === "a" ? "c" : "zz";
    out += String(o[k]) + ";";
  }
  let j: string;
  for (j in o) out += String(o[j]);
  return out + j;
}

function symbols(): string {
  const s = Symbol("s");
  const o = { a: 1, [s]: 2, b: 3 } as Record<string, number>;
  let out = "";
  for (const k in o) out += k + "=" + String(o[k]) + ";";
  return out;
}

function shapeChange(): string {
  const o: Record<string, number> = { a: 1, b: 2, c: 3 };
  let out = "";
  for (const k in o) {
    out += k + "=" + String(o[k]) + ";";
    o.a = 50;
    o.c = 70;
    Object.defineProperty(o, "b", { get: () => 99 });
  }
  return out;
}

function dict(): string {
  const o: Record<string, number> = {};
  for (let i = 0; i < 40; i++) o["k" + i] = i;
  for (let i = 0; i < 40; i += 3) delete o["k" + i];
  let s = 0;
  let n = 0;
  for (const k in o) {
    s += o[k];
    n++;
  }
  const arr = [1, 2, 3] as unknown as Record<string, number>;
  let t = 0;
  for (const k in arr) t += arr[k];
  const str = new String("hey") as unknown as Record<string, string>;
  let u = "";
  for (const k in str) u += str[k];
  return s + "," + n + "," + t + "," + u;
}

const o: Record<string, number> = {};
for (let i = 0; i < 20; i++) o["k" + i] = i;
console.log(sum(o), sum({}), sum({ x: 1.5, y: 2 }));
console.log(deleting());
console.log(adding());
console.log(protoKeys());
console.log(numeric());
console.log(getters());
console.log(otherObj());
console.log(reassigned());
console.log(symbols());
console.log(shapeChange());
console.log(dict());
