import { hypot, NAME } from "@acme/geom";
import { Circle } from "@acme/geom/shapes/circle";
import tiny from "tiny";
import { plain } from "plain";

export function report(): string {
  const c = new Circle(2);
  return [hypot(3, 4), NAME, c.area().toFixed(3), c.tag(), tiny("x"), plain].join(" ");
}
