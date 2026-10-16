// @native f g C make sources
// Function.prototype.toString of native functions: the source text, as the
// interpreted program's (without types; closures printed on their own, so
// up to indentation), not `[native code]`.
const f = ({ params: { id } }: { params: { id: string } }) => ({ id, name: "user" + id });
function g(a: number): number {
  return a + 1;
}
async function* gen(n: number) {
  yield n;
}
class C {
  m(x: number) {
    return x * 2;
  }
  get v(): number {
    return 1;
  }
  static s() {
    return 2;
  }
}
function make(k: number) {
  const inner = (x: number): number => x * k;
  const obj = {
    m(y: number) {
      return y + k;
    },
    get g() {
      return k;
    },
    async a(z: string) {
      return z;
    },
  };
  async function named(q: number) {
    return q;
  }
  const typed = <T,>(xs: T[]): T | undefined => xs[0] as T;
  return { inner, obj, named, typed };
}

function sources(): string[] {
  const r = make(3);
  const fns: Function[] = [
    f,
    g,
    gen,
    C.prototype.m,
    Object.getOwnPropertyDescriptor(C.prototype, "v")!.get!,
    C.s,
    r.inner,
    r.obj.m,
    Object.getOwnPropertyDescriptor(r.obj, "g")!.get!,
    r.obj.a,
    r.named,
    r.typed,
  ];
  // (A function's own toString does not change what Function.prototype's gives.)
  (g as any).toString = () => "own";
  return fns.map((fn) => Function.prototype.toString.call(fn).replace(/\s+/g, " "));
}

for (const s of sources()) console.log(s);
console.log(String(g), make(2).inner(4));
