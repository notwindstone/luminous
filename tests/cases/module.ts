// @native scale area isBig
const FACTOR = 3;
const NEG = -2.5;
const ON = true;
export function scale(x: number): number { return x * FACTOR + NEG; }
export function area(r: number): number { return Math.PI * r * r; }
function isBig(x: number): boolean { return ON && x > 100; }
console.log(scale(2), area(2), isBig(50), isBig(500));
interface Point { x: number; y: number }
const p: Point = { x: 1, y: 2 };
console.log(scale(p.x + p.y));
