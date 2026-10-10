// @native root
// Math / Number are locked before any code runs, in both build modes.
// Patches the compiler cannot see statically still fail at runtime.
function root(x: number): number { return Math.sqrt(x); }

const anyMath = Math as unknown as Record<string, unknown>;
const key = ["sq", "rt"].join("");
const attempts: [string, () => void][] = [
  ["assign", () => { anyMath[key] = () => 42; }],
  ["define", () => { Object.defineProperty(anyMath, key, { value: () => 42 }); }],
  ["delete", () => { delete anyMath[key]; }],
  ["constant", () => { anyMath["P" + "I"] = 3; }],
  ["number", () => { (Number as unknown as Record<string, unknown>)["is" + "NaN"] = () => true; }],
  ["global", () => { (globalThis as Record<string, unknown>)["Ma" + "th"] = {}; }],
];
for (const [name, f] of attempts) {
  try {
    f();
    console.log(name, "succeeded");
  } catch (e) {
    console.log(name, (e as Error).name);
  }
}
// Extending the builtins is still allowed.
anyMath["tau"] = 2 * Math.PI;
console.log(anyMath["tau"], root(16), Math.sqrt(2), Number.isNaN(NaN));
