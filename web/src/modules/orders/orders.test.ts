import { describe, expect, it } from "vitest";
import { ZodError } from "zod";
import { priceCart, PricingError, type CatalogueProduct, type PromoRule } from "./pricing";
import {
  makeReference,
  placeOrder,
  quoteOrder,
  type CreatedOrder,
  type NewOrderData,
  type OrderRepository,
} from "./order.service";
import { canTransition, holdsStock, isTerminal } from "./status";

const burger: CatalogueProduct = {
  id: "burger",
  name: "Smash Burger",
  basePrice: 1500,
  promoPrice: null,
  available: true,
  sizes: [
    { name: "Regular", priceDelta: 0 },
    { name: "Large", priceDelta: 300 },
  ],
  extras: [
    { name: "Cheese", price: 200, available: true },
    { name: "Bacon", price: 250, available: false },
  ],
};
const shake: CatalogueProduct = {
  id: "shake",
  name: "Mango Shake",
  basePrice: 1000,
  promoPrice: 800,
  available: true,
  sizes: [],
  extras: [],
};
const soldOut: CatalogueProduct = { ...burger, id: "gone", name: "Sold out", available: false };

const catalogue = {
  products: new Map([burger, shake, soldOut].map((p) => [p.id, p])),
};
const delivery = { freeOver: 3000, fee: 499 };

const promo = (over: Partial<PromoRule> = {}): PromoRule => ({
  code: "SAVE10",
  type: "PERCENT",
  value: 10,
  minSubtotal: 0,
  maxUses: null,
  usedCount: 0,
  startsAt: null,
  endsAt: null,
  active: true,
  ...over,
});

const price = (lines: Parameters<typeof priceCart>[0]["lines"], extra: Partial<Parameters<typeof priceCart>[0]> = {}) =>
  priceCart({ catalogue, lines, fulfilment: "COLLECTION", delivery, ...extra });

describe("priceCart", () => {
  it("prices size and extras from the catalogue", () => {
    const r = price([{ productId: "burger", quantity: 2, size: "Large", extras: ["Cheese"] }]);
    expect(r.lines[0].unitPrice).toBe(1500 + 300 + 200);
    expect(r.subtotal).toBe(4000);
    expect(r.total).toBe(4000);
  });

  it("uses the promotional price when set", () => {
    expect(price([{ productId: "shake", quantity: 1 }]).subtotal).toBe(800);
  });

  it("defaults to the first size and ignores duplicate extras", () => {
    const r = price([{ productId: "burger", quantity: 1, extras: ["Cheese", "Cheese"] }]);
    expect(r.lines[0].sizeName).toBe("Regular");
    expect(r.lines[0].extras).toEqual(["Cheese"]);
    expect(r.subtotal).toBe(1700);
  });

  it.each([
    [{ productId: "nope", quantity: 1 }, "UNKNOWN_PRODUCT"],
    [{ productId: "gone", quantity: 1 }, "PRODUCT_UNAVAILABLE"],
    [{ productId: "burger", quantity: 0 }, "INVALID_QUANTITY"],
    [{ productId: "burger", quantity: 1.5 }, "INVALID_QUANTITY"],
    [{ productId: "burger", quantity: 51 }, "INVALID_QUANTITY"],
    [{ productId: "burger", quantity: 1, size: "Huge" }, "INVALID_SIZE"],
    [{ productId: "shake", quantity: 1, size: "Large" }, "INVALID_SIZE"],
    [{ productId: "burger", quantity: 1, extras: ["Bacon"] }, "INVALID_EXTRA"],
    [{ productId: "burger", quantity: 1, extras: ["Caviar"] }, "INVALID_EXTRA"],
  ])("rejects %j with %s", (line, code) => {
    expect(() => price([line])).toThrowError(expect.objectContaining({ code }));
  });

  it("rejects an empty cart", () => {
    expect(() => price([])).toThrow(PricingError);
  });

  it("charges delivery below the free threshold and not above it", () => {
    const below = price([{ productId: "burger", quantity: 1 }], { fulfilment: "DELIVERY" });
    expect(below.deliveryFee).toBe(499);
    expect(below.total).toBe(1500 + 499);
    const above = price([{ productId: "burger", quantity: 2 }], { fulfilment: "DELIVERY" });
    expect(above.deliveryFee).toBe(0);
  });

  it("never charges delivery for collection or dine-in", () => {
    expect(price([{ productId: "burger", quantity: 1 }], { fulfilment: "DINE_IN" }).deliveryFee).toBe(0);
  });

  it("applies percent and fixed promos without going below zero", () => {
    expect(price([{ productId: "burger", quantity: 2 }], { promo: promo() }).discount).toBe(300);
    const fixed = price([{ productId: "shake", quantity: 1 }], { promo: promo({ type: "FIXED", value: 5000 }) });
    expect(fixed.discount).toBe(800);
    expect(fixed.total).toBe(0);
  });

  it.each([
    [promo({ active: false }), "PROMO_INVALID"],
    [promo({ endsAt: new Date("2000-01-01") }), "PROMO_EXPIRED"],
    [promo({ startsAt: new Date("2999-01-01") }), "PROMO_EXPIRED"],
    [promo({ maxUses: 5, usedCount: 5 }), "PROMO_EXHAUSTED"],
    [promo({ minSubtotal: 99999 }), "PROMO_MIN_SUBTOTAL"],
  ])("rejects an unusable promo (%#)", (p, code) => {
    expect(() => price([{ productId: "burger", quantity: 1 }], { promo: p })).toThrowError(expect.objectContaining({ code }));
  });
});

/** In-memory repository standing in for the database. */
function fakeRepo(promos: PromoRule[] = [promo()]) {
  const orders: (NewOrderData & { id: string; reference: string })[] = [];
  const repo: OrderRepository = {
    defaultBranchId: async () => "branch-1",
    deliveryConfig: async () => delivery,
    loadCatalogue: async (ids) => ({ products: new Map(ids.filter((i) => catalogue.products.has(i)).map((i) => [i, catalogue.products.get(i)!])) }),
    findPromo: async (code) => promos.find((p) => p.code === code) ?? null,
    findByIdempotencyKey: async (key) => {
      const o = orders.find((x) => x.idempotencyKey === key);
      return o ? ({ id: o.id, reference: o.reference, status: "PENDING", paymentStatus: "UNPAID", total: o.priced.total } satisfies CreatedOrder) : null;
    },
    createOrder: async (data, newReference) => {
      const created = { ...data, id: `o${orders.length + 1}`, reference: newReference() };
      orders.push(created);
      return { id: created.id, reference: created.reference, status: "PENDING", paymentStatus: "UNPAID", total: data.priced.total };
    },
  };
  return { repo, orders };
}

const validOrder = {
  idempotencyKey: "key-12345678",
  lines: [{ productId: "burger", quantity: 1 }],
  fulfilment: "COLLECTION",
  customerName: "Ama Mensah",
  customerPhone: "+233 24 000 0000",
};

describe("placeOrder", () => {
  it("ignores any price sent by the browser", async () => {
    const { repo, orders } = fakeRepo();
    const tampered = { ...validOrder, lines: [{ productId: "burger", quantity: 1, unitPrice: 1, total: 1 }], total: 1, subtotal: 1 };
    const { order } = await placeOrder(repo, tampered);
    expect(order.total).toBe(1500);
    expect(orders[0].priced.subtotal).toBe(1500);
  });

  it("returns the same order when the idempotency key is replayed", async () => {
    const { repo, orders } = fakeRepo();
    const first = await placeOrder(repo, validOrder);
    const second = await placeOrder(repo, validOrder);
    expect(first.replayed).toBe(false);
    expect(second.replayed).toBe(true);
    expect(second.order.id).toBe(first.order.id);
    expect(orders).toHaveLength(1);
  });

  it("requires an address for delivery", async () => {
    const { repo } = fakeRepo();
    await expect(placeOrder(repo, { ...validOrder, fulfilment: "DELIVERY" })).rejects.toBeInstanceOf(ZodError);
    const ok = await placeOrder(repo, { ...validOrder, idempotencyKey: "key-87654321", fulfilment: "DELIVERY", deliveryAddress: "12 Test Road, Accra" });
    expect(ok.priced?.deliveryFee).toBe(499);
  });

  it.each([
    [{ customerName: "A" }],
    [{ customerPhone: "123" }],
    [{ customerEmail: "not-an-email" }],
    [{ idempotencyKey: "short" }],
    [{ lines: [] }],
  ])("rejects invalid input %j", async (bad) => {
    const { repo, orders } = fakeRepo();
    await expect(placeOrder(repo, { ...validOrder, ...bad })).rejects.toBeInstanceOf(ZodError);
    expect(orders).toHaveLength(0);
  });

  it("rejects an unknown promo code and unavailable products", async () => {
    const { repo } = fakeRepo();
    await expect(placeOrder(repo, { ...validOrder, promoCode: "NOPE" })).rejects.toMatchObject({ code: "PROMO_INVALID" });
    await expect(placeOrder(repo, { ...validOrder, lines: [{ productId: "gone", quantity: 1 }] })).rejects.toMatchObject({ code: "PRODUCT_UNAVAILABLE" });
  });

  it("applies a valid promo case-insensitively when quoting", async () => {
    const { repo } = fakeRepo();
    const q = await quoteOrder(repo, { lines: [{ productId: "burger", quantity: 2 }], fulfilment: "COLLECTION", promoCode: "save10" });
    expect(q.discount).toBe(300);
    expect(q.total).toBe(2700);
  });
});

describe("order references", () => {
  it("are prefixed and look unique", () => {
    const a = makeReference(new Date("2026-11-02T10:00:00Z"), () => 0.1);
    const b = makeReference(new Date("2026-11-02T10:00:01Z"), () => 0.1);
    expect(a).toMatch(/^BO-[0-9A-Z]{8}$/);
    expect(a).not.toBe(b);
  });
});

describe("order status flow", () => {
  it("allows the normal lifecycle", () => {
    expect(canTransition("PENDING", "CONFIRMED")).toBe(true);
    expect(canTransition("CONFIRMED", "PREPARING")).toBe(true);
    expect(canTransition("PREPARING", "READY")).toBe(true);
    expect(canTransition("READY", "OUT_FOR_DELIVERY")).toBe(true);
    expect(canTransition("OUT_FOR_DELIVERY", "COMPLETED")).toBe(true);
  });
  it("blocks skipping steps and reopening finished orders", () => {
    expect(canTransition("PENDING", "COMPLETED")).toBe(false);
    expect(canTransition("COMPLETED", "PREPARING")).toBe(false);
    expect(canTransition("CANCELLED", "CONFIRMED")).toBe(false);
    expect(isTerminal("COMPLETED")).toBe(true);
    expect(isTerminal("CANCELLED")).toBe(true);
  });
  it("only holds stock for confirmed, active orders", () => {
    expect(holdsStock("PENDING")).toBe(false);
    expect(holdsStock("PREPARING")).toBe(true);
    expect(holdsStock("CANCELLED")).toBe(false);
  });
});
