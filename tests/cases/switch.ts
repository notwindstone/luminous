// @native classify fall loopSwitch
aot function classify(x: unknown): string {
  switch (x) {
    case 1:
      return "one";
    case "1":
      return "string one";
    case null:
      return "null";
    default:
      return "other";
    case 2:
      return "two";
  }
}
aot function fall(n: number): string {
  let s = "";
  switch (n) {
    case 0:
      s += "zero ";
    case 1:
      s += "one ";
      break;
    default:
      s += "default ";
    case 3:
      s += "three ";
  }
  return s;
}
aot function loopSwitch(): string {
  let s = "";
  for (let i = 0; i < 6; i++) {
    switch (i % 3) {
      case 0:
        continue;
      case 1:
        s += "a" + i;
        break;
      default:
        s += "b" + i;
    }
    s += ";";
  }
  return s;
}
for (const x of [1, "1", 2, null, undefined, 1.0, "x"]) console.log(classify(x));
console.log(fall(0), "|", fall(1), "|", fall(2), "|", fall(3), "|", loopSwitch());
