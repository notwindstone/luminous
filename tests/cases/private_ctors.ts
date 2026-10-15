// @native Account Savings Context summary has peek tryRecord #record accounts contexts awaitInto
// Native constructors of classes with private fields, private methods
// (the instance's brand) and fields that are arrow functions seeing the
// instance, as frameworks write them (Hono's Context).

class Account {
  #balance = 0;
  #log: string[] = [];
  #owner: string;
  id: number;
  deposit = (n: number) => {
    this.#balance += n;
    this.#record("+" + n);
    return this;
  };
  #audit = () => this.#log.join(" ");
  constructor(owner: string, id = 1) {
    this.#owner = owner;
    this.id = id;
  }
  #record(s: string) {
    this.#log.push(s);
  }
  get #summary() {
    return this.#owner + ":" + this.#balance;
  }
  summary() {
    return this.#summary + " [" + this.#audit() + "]";
  }
  static has(o: object) {
    return #balance in o;
  }
  // A private method called on an object without the brand: a TypeError
  // before the arguments run.
  static tryRecord(o: any) {
    const seen: string[] = [];
    try {
      o.#record((seen.push("arg"), "x"));
      return "ok " + seen.length;
    } catch (e) {
      return (e as Error).constructor.name + " " + seen.length;
    }
  }
  async #later(n: number) {
    await null;
    return this.#balance + n;
  }
  later(n: number) {
    return this.#later(n);
  }
  static peek(o: any) {
    try {
      return o.#summary;
    } catch (e) {
      return (e as Error).constructor.name;
    }
  }
}

class Savings extends Account {
  #rate: number;
  bonus = this.id * 10;
  apply = () => this.deposit(this.#rate);
  constructor(owner: string, rate: number) {
    super(owner, 7);
    this.#rate = rate;
  }
  get rate() {
    return this.#rate;
  }
}

var Context = class {
  #req: string;
  #status: number | undefined;
  #headers: Record<string, string> | undefined;
  finalized = false;
  env = {};
  header = (name: string, value: string) => {
    (this.#headers ??= {})[name] = value;
  };
  status = (s: number) => {
    this.#status = s;
  };
  json = (o: unknown) => {
    this.finalized = true;
    return JSON.stringify({ req: this.#req, status: this.#status ?? 200, headers: this.#headers ?? null, body: o });
  };
  constructor(req: string) {
    this.#req = req;
  }
};

function accounts(): string {
  const a = new Account("ann");
  a.deposit(5).deposit(7);
  const s = new Savings("sam", 3);
  s.apply();
  s.apply();
  return [
    a.summary(),
    s.summary(),
    s.rate,
    s.bonus,
    Account.has(a),
    Account.has({}),
    Account.peek(s),
    Account.peek({}),
    Object.keys(a).join(","),
    Object.keys(s).join(","),
    JSON.stringify(a),
    a.deposit.name,
    Account.tryRecord(a),
    Account.tryRecord({}),
  ].join(" | ");
}

function contexts(): string {
  let last = "";
  let n = 0;
  for (let i = 0; i < 2000; i++) {
    const c = new Context("/r" + (i % 10));
    if (i % 3 === 0) c.header("x-i", String(i));
    c.status(200 + (i % 2));
    last = c.json({ i });
    if (c.finalized) n++;
  }
  const c = new Context("/k");
  return [last, n, Object.keys(c).join(","), c.header.name, c.json.name].join(" | ");
}

console.log(accounts());
console.log(contexts());
new Account("lee").deposit(4).later(1).then((v) => console.log("later", v));

// `await` assigned to a property, a module binding or an element.
let awaitedModule = 0;
async function awaitInto(): Promise<string> {
  const o: Record<string, number> = {};
  const xs = [0, 0];
  o.k = await Promise.resolve(1);
  awaitedModule = await Promise.resolve(2);
  xs[1] = await Promise.resolve(3);
  return [o.k, awaitedModule, xs.join(",")].join(" ");
}
awaitInto().then((s) => console.log("awaited", s));
