import { SessionStore } from "./session";

export type Route = { method: "GET" | "POST"; path: string; handler: string };

export const routeTable: Route[] = [
  { method: "GET", path: "/dashboard", handler: "renderDashboard" },
  { method: "POST", path: "/login", handler: "handleLogin" },
];

export function handleNotFound(path: string): number {
  console.warn(`no route for ${path}`);
  return 404;
}

export function requireSession(store: SessionStore, token: string): boolean {
  return store.lookupSession(token) !== undefined;
}
