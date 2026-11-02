import { NextResponse } from "next/server";
import { ZodError } from "zod";
import { PricingError } from "@/modules/orders/pricing";

/** Central error mapping so route handlers never leak internals. */
export function handleError(e: unknown): NextResponse {
  if (e instanceof ZodError) {
    return NextResponse.json(
      { error: "VALIDATION_FAILED", issues: e.issues.map((i) => ({ path: i.path.join("."), message: i.message })) },
      { status: 422 },
    );
  }
  if (e instanceof PricingError) {
    return NextResponse.json({ error: e.code, message: e.message }, { status: 409 });
  }
  console.error("Unhandled API error", e);
  return NextResponse.json({ error: "INTERNAL_ERROR", message: "Something went wrong." }, { status: 500 });
}

/** Parse a JSON body, rejecting oversized or malformed payloads. */
export async function readJson(req: Request, maxBytes = 50_000): Promise<unknown> {
  const text = await req.text();
  if (text.length > maxBytes) throw new ZodError([{ code: "custom", path: [], message: "Request too large" }]);
  try {
    return JSON.parse(text);
  } catch {
    throw new ZodError([{ code: "custom", path: [], message: "Invalid JSON" }]);
  }
}
