import { describe, expect, it } from "vitest";
import { cookieFrom, issueSession, LoginThrottle, passwordMatches, sessionValid, STAFF_COOKIE } from "./auth";

const env = { STAFF_PASSWORD: "correct horse battery", STAFF_SESSION_SECRET: "s".repeat(40) } as unknown as NodeJS.ProcessEnv;

describe("staff password", () => {
  it("accepts only the configured password", () => {
    expect(passwordMatches("correct horse battery", env)).toBe(true);
    expect(passwordMatches("correct horse batter", env)).toBe(false);
    expect(passwordMatches("", env)).toBe(false);
  });

  it("refuses to run with a weak or missing configuration", () => {
    expect(() => passwordMatches("x", { STAFF_SESSION_SECRET: "s".repeat(40) } as unknown as NodeJS.ProcessEnv)).toThrow(/STAFF_PASSWORD/);
    expect(() => passwordMatches("x", { STAFF_PASSWORD: "short", STAFF_SESSION_SECRET: "s".repeat(40) } as unknown as NodeJS.ProcessEnv)).toThrow(/STAFF_PASSWORD/);
    expect(() => passwordMatches("x", { STAFF_PASSWORD: "long enough password" } as unknown as NodeJS.ProcessEnv)).toThrow(/STAFF_SESSION_SECRET/);
  });
});

describe("staff session", () => {
  const now = 1_800_000_000_000;

  it("is valid until it expires", () => {
    const t = issueSession(now, env);
    expect(sessionValid(t, now + 1000, env)).toBe(true);
    expect(sessionValid(t, now + 12 * 3600_000 + 1, env)).toBe(false);
  });

  it("rejects tampering, other secrets and junk", () => {
    const t = issueSession(now, env);
    const [exp, sig] = t.split(".");
    expect(sessionValid(`${Number(exp) + 3600_000}.${sig}`, now, env)).toBe(false); // extended expiry
    expect(sessionValid(`${exp}.${sig.slice(0, -1)}${sig.endsWith("0") ? "1" : "0"}`, now, env)).toBe(false);
    expect(sessionValid(t, now, { ...env, STAFF_SESSION_SECRET: "t".repeat(40) } as NodeJS.ProcessEnv)).toBe(false);
    for (const junk of [undefined, "", "abc", "1.2", `${exp}.`, `x.${sig}`]) expect(sessionValid(junk, now, env)).toBe(false);
  });

  it("reads the cookie from a request", () => {
    const req = new Request("http://x", { headers: { cookie: `a=1; ${STAFF_COOKIE}=123.abc; b=2` } });
    expect(cookieFrom(req)).toBe("123.abc");
    expect(cookieFrom(new Request("http://x"))).toBeUndefined();
  });
});

describe("sign-in throttle", () => {
  it("locks after five failures for fifteen minutes, and success resets it", () => {
    let t = 0;
    const th = new LoginThrottle(() => t);
    for (let i = 0; i < 4; i++) th.fail();
    expect(th.lockedFor()).toBe(0);
    th.fail();
    expect(th.lockedFor()).toBe(15 * 60_000);
    t += 15 * 60_000;
    expect(th.lockedFor()).toBe(0);
    th.fail();
    th.succeed();
    expect(th.lockedFor()).toBe(0);
  });
});
