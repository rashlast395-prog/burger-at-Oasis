import { NextResponse } from "next/server";
import { quoteOrder } from "@/modules/orders/order.service";
import { getOrderRepository } from "@/server/repositories";
import { handleError, readJson } from "@/server/http";

export async function POST(req: Request) {
  try {
    const priced = await quoteOrder(await getOrderRepository(), await readJson(req));
    return NextResponse.json(priced);
  } catch (e) {
    return handleError(e);
  }
}
