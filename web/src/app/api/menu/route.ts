import { NextResponse } from "next/server";
import { handleError } from "@/server/http";
import { getMenu } from "@/server/menu";

// Route handlers are dynamic by default in this Next.js version, so this always reads live data.

export async function GET() {
  try {
    return NextResponse.json(await getMenu());
  } catch (e) {
    return handleError(e);
  }
}
