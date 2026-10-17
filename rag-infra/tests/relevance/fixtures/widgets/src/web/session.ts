// Browser session handling for the widget dashboard.

export const SESSION_COOKIE_NAME = "widget_session";

export interface SessionRecord {
  userId: string;
  issuedAt: number;
}

export function sessionCookieName(): string {
  return SESSION_COOKIE_NAME;
}

export class SessionStore {
  private records = new Map<string, SessionRecord>();

  createSession(userId: string): string {
    const token = crypto.randomUUID();
    this.records.set(token, { userId, issuedAt: Date.now() });
    return token;
  }

  lookupSession(token: string): SessionRecord | undefined {
    return this.records.get(token);
  }
}
