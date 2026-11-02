import { Prisma } from "@prisma/client";
import { db } from "@/server/db";
import type { CreatedOrder, NewOrderData, OrderRepository } from "@/modules/orders/order.service";
import type { CatalogueProduct, PromoRule } from "@/modules/orders/pricing";
import { deliveryConfigFromEnv } from "@/server/delivery";

const toCreated = (o: { id: string; reference: string; status: string; paymentStatus: string; total: number }): CreatedOrder => ({
  id: o.id,
  reference: o.reference,
  status: o.status,
  paymentStatus: o.paymentStatus,
  total: o.total,
});

export const prismaOrderRepository: OrderRepository = {
  async defaultBranchId() {
    const branch = await db.branch.findFirst({ where: { active: true }, orderBy: { createdAt: "asc" } });
    if (!branch) throw new Error("No active branch configured. Run the seed script.");
    return branch.id;
  },

  async deliveryConfig() {
    return deliveryConfigFromEnv();
  },

  async loadCatalogue(productIds) {
    const rows = await db.product.findMany({
      where: { id: { in: productIds } },
      include: { sizes: true, extras: true },
    });
    const products = new Map<string, CatalogueProduct>();
    for (const p of rows) {
      products.set(p.id, {
        id: p.id,
        name: p.name,
        basePrice: p.basePrice,
        promoPrice: p.promoPrice,
        available: p.available,
        sizes: p.sizes.map((s) => ({ name: s.name, priceDelta: s.priceDelta })),
        extras: p.extras.map((e) => ({ name: e.name, price: e.price, available: e.available })),
      });
    }
    return { products };
  },

  async findPromo(code): Promise<PromoRule | null> {
    const p = await db.promoCode.findUnique({ where: { code } });
    return p
      ? {
          code: p.code,
          type: p.type,
          value: p.value,
          minSubtotal: p.minSubtotal,
          maxUses: p.maxUses,
          usedCount: p.usedCount,
          startsAt: p.startsAt,
          endsAt: p.endsAt,
          active: p.active,
        }
      : null;
  },

  async findByIdempotencyKey(key) {
    const o = await db.order.findUnique({ where: { idempotencyKey: key } });
    return o ? toCreated(o) : null;
  },

  async createOrder(data: NewOrderData, newReference) {
    for (let attempt = 0; attempt < 5; attempt++) {
      try {
        const order = await db.$transaction(async (tx) => {
          if (data.priced.promoCode) {
            // Conditional increment so two simultaneous orders cannot exceed maxUses.
            const promo = await tx.promoCode.findUnique({ where: { code: data.priced.promoCode } });
            if (promo?.maxUses != null) {
              const res = await tx.promoCode.updateMany({
                where: { code: promo.code, usedCount: { lt: promo.maxUses } },
                data: { usedCount: { increment: 1 } },
              });
              if (res.count === 0) throw new Error("PROMO_EXHAUSTED");
            } else {
              await tx.promoCode.update({ where: { code: data.priced.promoCode }, data: { usedCount: { increment: 1 } } });
            }
          }
          return tx.order.create({
            data: {
              reference: newReference(),
              idempotencyKey: data.idempotencyKey,
              branchId: data.branchId,
              channel: data.channel,
              customerId: data.customerId,
              customerName: data.customerName,
              customerPhone: data.customerPhone,
              customerEmail: data.customerEmail,
              fulfilment: data.fulfilment,
              deliveryAddress: data.deliveryAddress,
              deliveryNotes: data.deliveryNotes,
              notes: data.notes,
              promoCode: data.priced.promoCode,
              subtotal: data.priced.subtotal,
              discount: data.priced.discount,
              deliveryFee: data.priced.deliveryFee,
              total: data.priced.total,
              items: {
                create: data.priced.lines.map((l) => ({
                  productId: l.productId,
                  name: l.name,
                  sizeName: l.sizeName,
                  spice: l.spice,
                  extras: l.extras,
                  quantity: l.quantity,
                  unitPrice: l.unitPrice,
                  lineTotal: l.lineTotal,
                  instructions: l.instructions,
                })),
              },
              events: { create: { type: "ORDER_CREATED", toStatus: "PENDING" } },
            },
          });
        });
        return toCreated(order);
      } catch (e) {
        if (e instanceof Prisma.PrismaClientKnownRequestError && e.code === "P2002") {
          const target = String(e.meta?.target ?? "");
          if (target.includes("idempotencyKey")) {
            // A concurrent request with the same key won the race: return its order.
            const existing = await db.order.findUnique({ where: { idempotencyKey: data.idempotencyKey } });
            if (existing) return toCreated(existing);
          }
          continue; // reference collision: try a new reference
        }
        throw e;
      }
    }
    throw new Error("Could not allocate a unique order reference.");
  },
};
