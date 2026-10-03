import { formatPrice } from "./price.ts";

export function total(items: number[]): string {
  return formatPrice(items.reduce((a, b) => a + b, 0));
}
