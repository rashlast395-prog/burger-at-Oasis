import { NextResponse } from "next/server";
import { clearedCookie, issueSession, loginThrottle, passwordMatches, sessionCookie } from "@/server/staff/auth";
import { handleError, readJson } from "@/server/http";

export async function POST(req: Request) {
  try {
    const wait = loginThrottle.lockedFor();
    if (wait > 0) {
      return NextResponse.json({ error: "LOCKED", message: `Too many attempts. Try again in ${Math.ceil(wait / 60000)} minute(s).` }, { status: 429 });
    }
    const body = (await readJson(req, 2_000)) as { password?: unknown };
    if (typeof body.password !== "string" || !passwordMatches(body.password)) {
      loginThrottle.fail();
      return NextResponse.json({ error: "WRONG_PASSWORD", message: "That password is not right." }, { status: 401 });
    }
    loginThrottle.succeed();
    return NextResponse.json({ ok: true }, { headers: { "Set-Cookie": sessionCookie(issueSession()) } });
  } catch (e) {
    return handleError(e);
  }
}

export async function DELETE() {
  return NextResponse.json({ ok: true }, { headers: { "Set-Cookie": clearedCookie } });
}
