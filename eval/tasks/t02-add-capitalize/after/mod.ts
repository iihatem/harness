export function lower(s: string): string {
  return s.toLowerCase();
}

export function capitalize(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}
