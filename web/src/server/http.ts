import { NextResponse } from "next/server";
import { ZodError } from "zod";
import { PricingError } from "@/modules/orders/pricing";
import { EngineError } from "@/server/engine/client";
import { StoreUnsupported } from "@/server/staff/orders";

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
  if (e instanceof EngineError && e.status < 500) {
    // The engine's refusals are written for people (e.g. "an order cannot go from 'placed' to 'completed'").
    return NextResponse.json({ error: e.code, message: e.message }, { status: e.status === 401 ? 500 : e.status });
  }
  if (e instanceof EngineError && e.code === "ENGINE_UNREACHABLE") {
    console.error(e.message);
    return NextResponse.json({ error: "UNAVAILABLE", message: "Ordering is temporarily unavailable. Please try again shortly." }, { status: 503 });
  }
  if (e instanceof StoreUnsupported) {
    return NextResponse.json({ error: "NOT_IMPLEMENTED", message: e.message }, { status: 501 });
  }
  if (e instanceof RangeError) {
    return NextResponse.json({ error: "INVALID", message: e.message }, { status: 422 });
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

export const unauthorised = () => NextResponse.json({ error: "UNAUTHORISED", message: "Please sign in." }, { status: 401 });
