import { NextResponse } from "next/server";
import { placeOrder } from "@/modules/orders/order.service";
import { getOrderRepository } from "@/server/repositories";
import { handleError, readJson } from "@/server/http";

export async function POST(req: Request) {
  try {
    const { order, replayed } = await placeOrder(await getOrderRepository(), await readJson(req));
    // 201 for a new order, 200 when the same idempotency key is replayed.
    return NextResponse.json({ order, replayed }, { status: replayed ? 200 : 201 });
  } catch (e) {
    return handleError(e);
  }
}
