/**
 * Staff sign-in for the order board. One shared staff password for now (from the
 * environment, never in code); per-person accounts and roles come with the full auth stage.
 *
 * The session is a signed cookie: `<expiry ms>.<HMAC-SHA256>`. It is HttpOnly (scripts cannot
 * read it), SameSite=Strict (other sites cannot make the browser send it) and expires after
 * 12 hours. Repeated wrong passwords lock sign-in for 15 minutes.
 */
import { createHmac, timingSafeEqual } from "node:crypto";

export const STAFF_COOKIE = "bo_staff";
const SESSION_MS = 12 * 60 * 60 * 1000;
const MAX_FAILURES = 5;
const LOCKOUT_MS = 15 * 60 * 1000;

function sessionSecret(env: NodeJS.ProcessEnv = process.env): string {
  const s = env.STAFF_SESSION_SECRET;
  if (!s || s.length < 32) throw new Error("STAFF_SESSION_SECRET must be set to a random value of at least 32 characters.");
  return s;
}

const hmac = (key: string, data: string) => createHmac("sha256", key).update(data).digest();

/** Constant-time password check (both sides hashed first so lengths do not leak). */
export function passwordMatches(given: string, env: NodeJS.ProcessEnv = process.env): boolean {
  const expected = env.STAFF_PASSWORD;
  if (!expected || expected.length < 12) throw new Error("STAFF_PASSWORD must be set (at least 12 characters).");
  const key = sessionSecret(env);
  return timingSafeEqual(hmac(key, `pw:${given}`), hmac(key, `pw:${expected}`));
}

export function issueSession(now = Date.now(), env: NodeJS.ProcessEnv = process.env): string {
  const exp = now + SESSION_MS;
  return `${exp}.${hmac(sessionSecret(env), `staff.${exp}`).toString("hex")}`;
}

export function sessionValid(token: string | undefined, now = Date.now(), env: NodeJS.ProcessEnv = process.env): boolean {
  if (!token) return false;
  const [expText, sig] = token.split(".");
  const exp = Number(expText);
  if (!Number.isFinite(exp) || exp <= now || !sig || !/^[0-9a-f]{64}$/.test(sig)) return false;
  return timingSafeEqual(Buffer.from(sig, "hex"), hmac(sessionSecret(env), `staff.${exp}`));
}

export function sessionCookie(token: string): string {
  const secure = process.env.NODE_ENV === "production" ? "; Secure" : "";
  return `${STAFF_COOKIE}=${token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=${SESSION_MS / 1000}${secure}`;
}

export const clearedCookie = `${STAFF_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0`;

export function cookieFrom(req: Request): string | undefined {
  const header = req.headers.get("cookie") ?? "";
  for (const part of header.split(";")) {
    const [k, ...v] = part.trim().split("=");
    if (k === STAFF_COOKIE) return v.join("=");
  }
  return undefined;
}

export const isStaff = (req: Request) => sessionValid(cookieFrom(req));

/** Sign-in throttle (per server process; enough for one restaurant's laptop or server). */
export class LoginThrottle {
  private failures: number[] = [];
  constructor(private readonly now: () => number = Date.now) {}

  lockedFor(): number {
    const t = this.now();
    this.failures = this.failures.filter((f) => t - f < LOCKOUT_MS);
    if (this.failures.length < MAX_FAILURES) return 0;
    return LOCKOUT_MS - (t - this.failures[this.failures.length - MAX_FAILURES]);
  }

  fail() {
    this.failures.push(this.now());
  }

  succeed() {
    this.failures = [];
  }
}

export const loginThrottle = new LoginThrottle();
