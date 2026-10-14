// @native Animal Dog Puppy Plain Point3 Logged Child Sealed SealedChild Fwd
// @native build keysOf order chain setters reproto sealed forward
// Constructors of derived classes and TS parameter properties, compiled
// natively: `super(...)` makes the instance with the superclass's
// constructor (compiled to make an instance of the subclass), then the
// subclass's fields are defined and its parameter properties assigned, in
// the order the erased JavaScript runs them. A class whose superclass was
// replaced (`Object.setPrototypeOf`) is constructed as interpreted code
// would.

const log: string[] = [];
function note(s: string): number {
  log.push(s);
  return log.length;
}

class Animal {
  legs = note("Animal.legs");
  declared: string;
  constructor(public name: string, readonly sound: string = "...") {
    note("Animal body " + name);
    this.declared = name + "!";
  }
  speak(): string {
    return this.name + " says " + this.sound;
  }
}

class Dog extends Animal {
  tricks: string[] = [];
  good = note("Dog.good");
  constructor(name: string, public breed: string) {
    super(name, "woof");
    note("Dog body " + this.legs);
    this.tricks.push("sit");
  }
}

class Puppy extends Dog {
  age = 0;
  constructor(name: string, private months: number = 2) {
    super(name.toUpperCase(), "mixed");
    this.age = this.months / 12;
  }
  speak(): string {
    return "yip! " + super.speak();
  }
}

// A derived class without a constructor: its arguments go on.
class Plain extends Dog {
  extra = "plain";
}

class Point3 {
  constructor(public x: number, public y: number, public z: number) {}
}

class Logged extends Point3 {
  constructor(x: number, y: number) {
    super(x, y, x + y);
    if (x < 0) return;
    note("Logged " + this.z);
  }
}

// The superclass's body adds a property before the subclass's fields.
class Parent {
  constructor(public a: number) {
    (this as any).added = a * 2;
  }
}
class Child extends Parent {
  b = 5;
  constructor(a: number, public c: number) {
    super(a);
  }
}

// A field of the subclass is defined, not assigned: the superclass's
// setter does not run; a parameter property is assigned: it does.
class Sealed {
  sets: string[] = [];
  set v(x: number) {
    this.sets.push("v=" + x);
  }
  set w(x: number) {
    this.sets.push("w=" + x);
  }
  constructor(lock: boolean) {
    if (lock) Object.preventExtensions(this);
  }
}
class SealedChild extends Sealed {
  v = 1;
  constructor(lock: boolean, public w: number) {
    super(lock);
  }
}

class Fwd extends Point3 {}

function build(): string {
  const d = new Dog("rex", "lab");
  const p = new Puppy("bo", 6);
  const q = new Puppy("al");
  return [d.speak(), p.speak(), q.speak(), p.age, q.age, d.tricks.join(), d instanceof Animal, p instanceof Dog].join(
    " | ",
  );
}

function keysOf(): string {
  const objs = [new Animal("a"), new Dog("d", "x"), new Puppy("p"), new Plain("pl", "mutt"), new Child(3, 4)];
  return objs.map((o) => Object.keys(o).join(",")).join(" / ");
}

function order(): string {
  log.length = 0;
  new Puppy("z", 3);
  return log.join(", ");
}

function chain(): string {
  let s = 0;
  for (let i = -2; i < 50; i++) {
    const l = new Logged(i, 1);
    s += l.x + l.y + l.z;
  }
  return s + " " + log.length + " " + JSON.stringify(new Logged(2, 3));
}

function setters(): string {
  const s = new SealedChild(false, 7);
  let r = JSON.stringify(s) + " " + s.sets.join();
  try {
    new SealedChild(true, 8);
    r += " no error";
  } catch (e) {
    r += " " + (e as Error).constructor.name;
  }
  return r;
}

class Other {
  constructor(a: number) {
    (this as any).other = a;
  }
}

function reproto(): string {
  const before = JSON.stringify(new Child(1, 2));
  Object.setPrototypeOf(Child, Other);
  const c = new Child(5, 6);
  Object.setPrototypeOf(Child, Parent);
  const after = JSON.stringify(new Child(7, 8));
  return [before, JSON.stringify(c), c instanceof Child, after].join(" ");
}

function sealed(): string {
  return JSON.stringify(new Sealed(true).sets);
}

function forward(): string {
  const f = new Fwd(1, 2, 3);
  const g = new Fwd(4);
  return JSON.stringify([f, g]) + " " + (f instanceof Point3);
}

console.log(build());
console.log(keysOf());
console.log(order());
console.log(chain());
console.log(setters());
console.log(reproto());
console.log(sealed());
console.log(forward());
// Interpreted construction of the same classes.
const viaReflect = Reflect.construct(Puppy, ["r", 12]) as Puppy;
console.log(JSON.stringify(viaReflect), viaReflect.speak());
