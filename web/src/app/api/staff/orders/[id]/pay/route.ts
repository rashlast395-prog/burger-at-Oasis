import { NextResponse } from "next/server";
import { isStaff } from "@/server/staff/auth";
import { takeCash } from "@/server/staff/orders";
import { handleError, readJson, unauthorised } from "@/server/http";

/** Record cash taken for an order (amount in pesewas). */
export async function POST(req: Request, { params }: { params: Promise<{ id: string }> }) {
  try {
    if (!isStaff(req)) return unauthorised();
    const { id } = await params;
    const body = (await readJson(req, 2_000)) as { amount?: unknown };
    return NextResponse.json({ payment: await takeCash(id, Number(body.amount)) });
  } catch (e) {
    return handleError(e);
  }
}
