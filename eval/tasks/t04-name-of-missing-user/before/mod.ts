export interface User {
  name?: string;
}

export function getName(user: User | undefined): string {
  return user.name;
}
