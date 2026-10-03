export function range(a: number, b: number): number[] {
  const out: number[] = [];
  for (let i = a; i <= b; i++) {
    out.push(i);
  }
  return out;
}
