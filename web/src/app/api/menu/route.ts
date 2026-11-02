import { NextResponse } from "next/server";
import { db } from "@/server/db";
import { handleError } from "@/server/http";

// Route handlers are dynamic by default in this Next.js version, so this always reads live data.

export async function GET() {
  try {
    const categories = await db.category.findMany({
      orderBy: { sortOrder: "asc" },
      include: {
        products: {
          where: { available: true },
          orderBy: { name: "asc" },
          include: { sizes: true, extras: { where: { available: true } } },
        },
      },
    });
    return NextResponse.json({ categories });
  } catch (e) {
    return handleError(e);
  }
}
