import { NextResponse } from "next/server";
import { isStaff } from "@/server/staff/auth";
import { changeStatus } from "@/server/staff/orders";
import { handleError, readJson, unauthorised } from "@/server/http";

export async function POST(req: Request, { params }: { params: Promise<{ id: string }> }) {
  try {
    if (!isStaff(req)) return unauthorised();
    const { id } = await params;
    const body = (await readJson(req, 2_000)) as { status?: unknown };
    return NextResponse.json({ order: await changeStatus(id, String(body.status ?? "")) });
  } catch (e) {
    return handleError(e);
  }
}
