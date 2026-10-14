import { twice } from "demo:native";
export function greet(name: string): string {
  return `hello, ${name} (${twice(21)})`;
}
