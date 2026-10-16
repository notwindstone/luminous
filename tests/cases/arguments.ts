// @native count first sum describe withDefaults withRest viaArrow nested gen later add group state #set main
// `arguments`: the arguments a function was called with (unmapped, as in
// strict code), also in the arrow functions inside it.
function count(a?: unknown, b?: unknown): number {
  return arguments.length;
}

function first(): unknown {
  return arguments[0];
}

function sum(): number {
  let s = 0;
  for (let i = 0; i < arguments.length; i++) s += arguments[i] as number;
  return s;
}

function describe(x?: unknown): string {
  const args = arguments;
  return [
    Object.prototype.toString.call(args),
    Array.isArray(args),
    typeof arguments,
    Array.from(args).join("|"),
    [...args].length,
    Object.keys(args).join(","),
    JSON.stringify(args),
  ].join(" ");
}

// Unmapped: assigning a parameter does not change `arguments`, nor the
// other way around; defaults are not arguments.
function withDefaults(a: number, b = 10): string {
  a = 5;
  (arguments as any)[1] = 99;
  return `${a} ${b} ${arguments.length} ${arguments[0]} ${arguments[1]}`;
}

function withRest(a: number, ...more: number[]): string {
  return `${arguments.length} ${more.length} ${arguments[arguments.length - 1]}`;
}

// Arrows use the enclosing function's.
function viaArrow(): number[] {
  const get = (i: number) => arguments[i] as number;
  const twice = () => () => arguments.length * 2;
  return [get(0), get(2), twice()()];
}

// A function inside has its own.
function nested(a: number): string {
  const inner = function (x: number) {
    return arguments.length + ":" + x;
  };
  return inner(a, 2, 3) + " " + arguments.length;
}

function* gen(): Generator<unknown> {
  for (let i = 0; i < arguments.length; i++) yield arguments[i];
}

async function later(): Promise<number> {
  await null;
  return arguments.length;
}

// As a framework's overloaded methods.
class Router {
  routes: string[] = [];
  store: Record<string, unknown> = {};
  add(method: string, path?: string, handler?: () => string): this {
    if (arguments.length === 1) this.routes.push("GET " + method);
    else this.routes.push(method + " " + path + (handler ? " " + handler() : ""));
    return this;
  }
  group(): this {
    const prefix = arguments[0] as string;
    const run = arguments[1] as (r: Router) => void;
    const sub = new Router();
    run(sub);
    for (const r of sub.routes) this.routes.push(r.replace(" ", " " + prefix));
    return this;
  }
  state(nameOrStore: string | Record<string, unknown>, value?: unknown): this {
    return this.#set(arguments.length, nameOrStore, value);
  }
  #set(n: number, a: string | Record<string, unknown>, b: unknown): this {
    if (n === 1) Object.assign(this.store, a);
    else this.store[a as string] = b;
    return this;
  }
}

async function main(): Promise<void> {
  console.log(count(), count(1), count(1, 2), count(1, 2, 3), count(undefined));
  console.log(first(), first("a", "b"));
  console.log(sum(), sum(1, 2, 3, 4));
  console.log(describe(1, "two", null));
  console.log(withDefaults(1), withDefaults(1, 2), withDefaults(1, undefined, 3));
  console.log(withRest(1), withRest(1, 2, 3));
  console.log(viaArrow(7, 8, 9).join(","));
  console.log(nested(1));
  console.log([...gen(1, 2, 3)].join(","));
  console.log(await later(), await later(1, 2));
  const r = new Router()
    .add("/")
    .add("POST", "/x", () => "h")
    .group("/api", (s: Router) => s.add("/users").add("PUT", "/items"))
    .state("a", 1)
    .state({ b: 2 })
    .state("c", undefined);
  console.log(r.routes.join("; "), JSON.stringify(r.store), "c" in r.store);
  // Called through call, apply and as a callback.
  console.log(sum.call(null, 1, 2), sum.apply(null, [3, 4, 5]), [1, 2].map(count as any).join(","));
  const callee = (function () {
    try {
      return (arguments as any).callee;
    } catch (e) {
      return (e as Error).constructor.name;
    }
  })();
  console.log(callee);
}

main();
