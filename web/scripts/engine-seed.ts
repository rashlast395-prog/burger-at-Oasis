/**
 * Loads the placeholder menu and promo codes into the engine (ORDER_STORE=engine).
 * Needs bo-server running with --api and the same BO_API_TOKEN in the environment.
 *   npm run engine:seed
 * Safe to re-run: products get a new version, promos are replaced.
 */
import { categories, EXTRAS, hasSizes, products, promos, SIZES } from "../prisma/menu-data";
import { engine } from "../src/server/engine/client";

async function main() {
  const catBySlug = new Map(categories.map((c) => [c.slug, c]));
  for (const p of products) {
    const cat = catBySlug.get(p.category);
    await engine("PUT", `/products/${encodeURIComponent(p.slug)}`, {
      name: p.name,
      description: p.description,
      category: p.category,
      category_name: cat?.name ?? p.category,
      category_order: cat?.sortOrder ?? 999,
      base_price: p.basePrice,
      promo_price: p.promoPrice,
      available: true,
      featured: p.featured,
      image_url: p.imageUrl,
      calories: p.calories,
      prep_minutes: p.prepMinutes,
      station: p.station,
      allergens: p.allergens,
      sizes: hasSizes(p.category) ? SIZES.map((s) => ({ name: s.name, price_delta: s.priceDelta })) : [],
      extras: EXTRAS.map((e) => ({ name: e.name, price: e.price, available: true })),
    });
  }
  for (const p of promos) {
    await engine("PUT", `/promos/${encodeURIComponent(p.code)}`, {
      type: p.type,
      value: p.value,
      min_subtotal: p.minSubtotal,
      max_uses: null,
      active: true,
    });
  }
  console.log(`Engine seeded: ${products.length} products, ${promos.length} promo codes.`);
}

main().catch((e) => {
  console.error(e instanceof Error ? e.message : e);
  process.exit(1);
});
