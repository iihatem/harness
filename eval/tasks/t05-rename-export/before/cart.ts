import { fmt } from "./price.ts";

export function total(items: number[]): string {
  return fmt(items.reduce((a, b) => a + b, 0));
}
