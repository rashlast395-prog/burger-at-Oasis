import { engine } from "@/server/engine/client";
import { orderStore } from "@/server/repositories";

/** The public menu in one shape, whichever store is active. Unavailable items are hidden. */
export interface MenuResponse {
  categories: {
    slug: string;
    name: string;
    sortOrder: number;
    products: {
      id: string;
      name: string;
      description: string | null;
      basePrice: number;
      promoPrice: number | null;
      imageUrl: string | null;
      sizes: { name: string; priceDelta: number }[];
      extras: { name: string; price: number }[];
    }[];
  }[];
}

interface EngineMenuProduct {
  _id: string;
  name: string;
  description?: string;
  category?: string;
  category_name?: string;
  category_order?: number;
  base_price: number;
  promo_price?: number | null;
  image_url?: string;
  available?: boolean;
  sizes?: { name: string; price_delta: number }[];
  extras?: { name: string; price: number; available?: boolean }[];
}

export function groupEngineMenu(products: EngineMenuProduct[]): MenuResponse {
  const cats = new Map<string, MenuResponse["categories"][number]>();
  for (const p of products) {
    if (p.available === false) continue;
    const slug = p.category ?? "other";
    if (!cats.has(slug)) {
      cats.set(slug, { slug, name: p.category_name ?? slug, sortOrder: p.category_order ?? 999, products: [] });
    }
    cats.get(slug)!.products.push({
      id: p._id,
      name: p.name,
      description: p.description ?? null,
      basePrice: p.base_price,
      promoPrice: p.promo_price ?? null,
      imageUrl: p.image_url ?? null,
      sizes: (p.sizes ?? []).map((s) => ({ name: s.name, priceDelta: s.price_delta })),
      extras: (p.extras ?? []).filter((e) => e.available !== false).map((e) => ({ name: e.name, price: e.price })),
    });
  }
  const categories = [...cats.values()].sort((a, b) => a.sortOrder - b.sortOrder);
  for (const c of categories) c.products.sort((a, b) => a.name.localeCompare(b.name));
  return { categories };
}

export async function getMenu(): Promise<MenuResponse> {
  if (orderStore() === "engine") {
    const res = await engine<{ products: EngineMenuProduct[] }>("GET", "/menu");
    return groupEngineMenu(res?.products ?? []);
  }
  const { db } = await import("@/server/db");
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
  return { categories } as unknown as MenuResponse;
}
