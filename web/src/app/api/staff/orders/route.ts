import { NextResponse } from "next/server";
import { isStaff } from "@/server/staff/auth";
import { listOrders } from "@/server/staff/orders";
import { handleError, unauthorised } from "@/server/http";

export async function GET(req: Request) {
  try {
    if (!isStaff(req)) return unauthorised();
    return NextResponse.json({ orders: await listOrders() });
  } catch (e) {
    return handleError(e);
  }
}
