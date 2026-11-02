import { NextResponse } from "next/server";
import { quoteOrder } from "@/modules/orders/order.service";
import { prismaOrderRepository } from "@/server/repositories/order.repository";
import { handleError, readJson } from "@/server/http";

export async function POST(req: Request) {
  try {
    const priced = await quoteOrder(prismaOrderRepository, await readJson(req));
    return NextResponse.json(priced);
  } catch (e) {
    return handleError(e);
  }
}
