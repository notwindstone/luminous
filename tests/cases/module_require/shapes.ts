// Only required lazily: its classes are assigned when it is evaluated.
console.log("shapes.ts evaluates");

export abstract class Shape {
  abstract area(): number;
  declare label: string | undefined;
  describe(): string {
    return `${this.constructor.name} ${this.area()}`;
  }
}

export class Square extends Shape {
  constructor(private side: number) {
    super();
  }
  area(): number {
    return this.side * this.side;
  }
}

function warn(msg: string) {
  if (warn.warned) return "quiet";
  warn.warned = true;
  return msg;
}
declare namespace warn {
  export let warned: boolean | undefined;
}

export { warn };
