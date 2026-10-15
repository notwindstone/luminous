import { twice } from "demo:native";
export function greet(name: string): string {
  return `hello, ${name} (${twice(21)})`;
}
export class Greeter {
  prefix: string;
  constructor(prefix: string) {
    this.prefix = prefix;
  }
  greet(name: string): string {
    return `${this.prefix}, ${name}`;
  }
}
// (A module's binding with the name of a global that the app declares.)
export const fromPrelude = "a binding of demo:greet";
