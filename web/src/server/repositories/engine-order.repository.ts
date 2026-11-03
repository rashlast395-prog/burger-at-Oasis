/**
 * OrderRepository backed by the Burger Oasis engine (experimental; laptop testing only until
 * the engine passes its acceptance tests). Pricing and validation stay in the order service;
 * this only stores and reads. The engine re-checks the total and enforces idempotency and
 * promo limits atomically, so the guarantees match the Prisma repository.
 */
import type { CreatedOrder, NewOrderData, OrderRepository } from "@/modules/orders/order.service";
import { PricingError, type CatalogueProduct, type PromoRule } from "@/modules/orders/pricing";
import { deliveryConfigFromEnv } from "@/server/delivery";
import { compact, engine, EngineError } from "@/server/engine/client";

/** Engine order status -> the platform's OrderStatus names. */
export const STATUS_FROM_ENGINE: Record<string, string> = {
  placed: "PENDING",
  accepted: "CONFIRMED",
  preparing: "PREPARING",
  ready: "READY",
  out_for_delivery: "OUT_FOR_DELIVERY",
  completed: "COMPLETED",
  cancelled: "CANCELLED",
};

/**
 * Engine payment status -> PaymentStatus. A partly paid order counts as UNPAID: treating it
 * as paid could release food or goods that were not paid for.
 */
export const PAYMENT_FROM_ENGINE: Record<string, string> = {
  unpaid: "UNPAID",
  partially_paid: "UNPAID",
  paid: "PAID",
  overpaid: "PAID",
  refunded: "REFUNDED",
};

interface EngineCreated {
  id: string;
  reference: string;
  status: string;
  payment_status: string;
  total: number;
}

interface EngineProduct {
  _id: string;
  name: string;
  base_price: number;
  promo_price?: number | null;
  available?: boolean;
  sizes?: { name: string; price_delta: number }[];
  extras?: { name: string; price: number; available?: boolean }[];
}

interface EnginePromo {
  _id: string;
  type: "PERCENT" | "FIXED";
  value: number;
  min_subtotal?: number;
  max_uses?: number | null;
  used_count: number;
  starts_at?: string | null;
  ends_at?: string | null;
  active?: boolean;
}

export function toCreated(o: EngineCreated): CreatedOrder {
  return {
    id: o.id,
    reference: o.reference,
    status: STATUS_FROM_ENGINE[o.status] ?? o.status.toUpperCase(),
    paymentStatus: PAYMENT_FROM_ENGINE[o.payment_status] ?? "UNPAID",
    total: o.total,
  };
}

export function toCatalogueProduct(p: EngineProduct): CatalogueProduct {
  return {
    id: p._id,
    name: p.name,
    basePrice: p.base_price,
    promoPrice: p.promo_price ?? null,
    available: p.available !== false,
    sizes: (p.sizes ?? []).map((s) => ({ name: s.name, priceDelta: s.price_delta })),
    extras: (p.extras ?? []).map((e) => ({ name: e.name, price: e.price, available: e.available !== false })),
  };
}

/** The order document the engine stores, built from an already-priced cart. */
export function toEngineOrder(data: NewOrderData, reference: string) {
  return compact({
    idempotency_key: data.idempotencyKey,
    reference,
    currency: process.env.BO_CURRENCY ?? "GHS",
    channel: data.channel,
    branch_id: data.branchId,
    customer_id: data.customerId,
    customer: compact({ name: data.customerName, phone: data.customerPhone, email: data.customerEmail }),
    fulfilment: data.fulfilment,
    delivery_address: data.deliveryAddress,
    delivery_notes: data.deliveryNotes,
    notes: data.notes,
    promo_code: data.priced.promoCode,
    items: data.priced.lines.map((l) =>
      compact({
        sku: l.productId,
        name: l.name,
        qty: l.quantity,
        unit_price: l.unitPrice,
        size: l.sizeName,
        spice: l.spice,
        extras: l.extras.length ? l.extras : null,
        instructions: l.instructions,
      }),
    ),
    delivery_fee: data.priced.deliveryFee,
    discount: data.priced.discount,
    expected_total: data.priced.total,
  });
}

export const engineOrderRepository: OrderRepository = {
  async defaultBranchId() {
    return process.env.BO_BRANCH_ID ?? "main";
  },

  async deliveryConfig() {
    return deliveryConfigFromEnv();
  },

  async loadCatalogue(productIds) {
    const res = await engine<{ products: EngineProduct[] }>(
      "GET",
      `/products?ids=${productIds.map(encodeURIComponent).join(",")}`,
    );
    return { products: new Map((res?.products ?? []).map((p) => [p._id, toCatalogueProduct(p)])) };
  },

  async findPromo(code): Promise<PromoRule | null> {
    const p = await engine<EnginePromo>("GET", `/promos/${encodeURIComponent(code)}`, undefined, { allowNotFound: true });
    if (!p) return null;
    return {
      code: p._id,
      type: p.type,
      value: p.value,
      minSubtotal: p.min_subtotal ?? 0,
      maxUses: p.max_uses ?? null,
      usedCount: p.used_count,
      startsAt: p.starts_at ? new Date(p.starts_at) : null,
      endsAt: p.ends_at ? new Date(p.ends_at) : null,
      active: p.active !== false,
    };
  },

  async findByIdempotencyKey(key) {
    const res = await engine<{ order: EngineCreated }>("GET", `/orders/by-key/${encodeURIComponent(key)}`, undefined, {
      allowNotFound: true,
    });
    return res ? toCreated(res.order) : null;
  },

  async createOrder(data, newReference) {
    try {
      const res = await engine<{ order: EngineCreated }>("POST", "/orders", toEngineOrder(data, newReference()));
      return toCreated(res!.order);
    } catch (e) {
      // Promo limits are enforced atomically by the engine; surface them like pricing errors.
      if (e instanceof EngineError && (e.code === "PROMO_EXHAUSTED" || e.code === "PROMO_INVALID")) {
        throw new PricingError(e.code, e.message);
      }
      throw e;
    }
  },
};
