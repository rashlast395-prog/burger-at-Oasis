/**
 * Seeds the PostgreSQL database with the placeholder menu (see menu-data.ts).
 * For the engine store use `npm run engine:seed` instead.
 */
import { PrismaClient } from "@prisma/client";
import { categories, EXTRAS, hasSizes, products, promos, SIZES } from "./menu-data";

const db = new PrismaClient();

async function main() {
  await db.branch.upsert({
    where: { slug: "main" },
    update: {},
    create: { name: "Burger Oasis", slug: "main" },
  });

  for (const c of categories) {
    await db.category.upsert({ where: { slug: c.slug }, update: { name: c.name, sortOrder: c.sortOrder }, create: c });
  }

  for (const p of products) {
    const category = await db.category.findUniqueOrThrow({ where: { slug: p.category } });
    const { category: _omit, ...data } = p;
    void _omit;
    const product = await db.product.upsert({
      where: { slug: p.slug },
      update: { ...data, categoryId: category.id },
      create: { ...data, categoryId: category.id },
    });
    if (hasSizes(p.category)) {
      for (const s of SIZES) {
        await db.productSize.upsert({
          where: { productId_name: { productId: product.id, name: s.name } },
          update: { priceDelta: s.priceDelta },
          create: { ...s, productId: product.id },
        });
      }
    }
    for (const e of EXTRAS) {
      await db.productExtra.upsert({
        where: { productId_name: { productId: product.id, name: e.name } },
        update: { price: e.price },
        create: { ...e, productId: product.id },
      });
    }
  }

  for (const p of promos) {
    await db.promoCode.upsert({ where: { code: p.code }, update: p, create: p });
  }
  console.log("Seed complete.");
}

main()
  .catch((e) => {
    console.error(e);
    process.exit(1);
  })
  .finally(() => db.$disconnect());
