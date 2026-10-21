// @native lens bytes rem
// A typed array's prototype properties through the caches: its getters
// (`length`, `byteLength`) on the second and third prototype (a Buffer's,
// as a subclass of Uint8Array: read directly while they are the
// intrinsic ones), methods, and its numeric keys (undefined, whatever the
// prototypes have); and `x % 1`.
class Fast extends Uint8Array {}
function lens(u: any): number {
  return u.byteLength + u.length;
}
function bytes(n: number): string {
  const a = new Uint8Array(n), f = new Fast(n), w = new Float64Array(n), d = new DataView(new ArrayBuffer(n));
  let s = 0;
  for (let i = 0; i < 100; i++) s += lens(a) + lens(f) + lens(w) + d.byteLength;
  const sub = f.subarray(1, 3);
  const proto: any = Object.getPrototypeOf(Uint8Array.prototype);
  (Fast.prototype as any)["1.5"] = "proto";
  (Fast.prototype as any).extra = "extra";
  const k = "1.5", m = "-0", e = "extra";
  const keys = [(f as any)[k], (f as any)[m], (f as any)[e], (f as any)["1.5"]].join(",");
  // A non-typed-array of the same shape: the getter throws.
  let err = "";
  try { lens(Object.create(Fast.prototype)); } catch (x) { err = (x as Error).constructor.name; }
  // Getters redefined (no shape change): the new ones are called.
  const desc = Object.getOwnPropertyDescriptor(proto, "byteLength")!;
  Object.defineProperty(proto, "byteLength", { get() { return 1000; }, configurable: true });
  const redefined = lens(f) + lens(a);
  Object.defineProperty(proto, "byteLength", desc);
  // An own property shadows them.
  Object.defineProperty(f, "length", { value: 7 });
  return [s, sub.length, sub.byteLength, keys, err, redefined, lens(f), lens(a)].join(" ");
}
console.log(bytes(8));
function rem(xs: number[]): string {
  return xs.map((x) => { const r = x % 1, q = x % -1; return Object.is(r, -0) ? "-0" : String(r) + "/" + (Object.is(q, -0) ? "-0" : String(q)); }).join(" ");
}
console.log(rem([4, -4, 1.5, -1.25, 0, -0, Infinity, -Infinity, NaN, 2 ** 60 + 0.5, 1e300]));
