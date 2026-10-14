import { setTimeout } from "qjs:os";
export function later(f: () => void): void { setTimeout(f, 0); }
