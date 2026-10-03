export function abs(x: number): number {
  return x < 0 ? -x : x;
}

export function clamp(x: number, lo: number, hi: number): number {
  return Math.min(Math.max(x, lo), hi);
}
