import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { NewOrderData } from "@/modules/orders/order.service";
import { PricingError } from "@/modules/orders/pricing";
import { EngineError } from "@/server/engine/client";
import { groupEngineMenu } from "@/server/menu";
import { engineOrderRepository, toCreated, toEngineOrder } from "./engine-order.repository";
import { orderStore } from "./index";

const data: NewOrderData = {
  idempotencyKey: "key-12345678",
  branchId: "main",
  channel: "WEB",
  customerId: null,
  customerName: "Ama Mensah",
  customerPhone: "0240000000",
  customerEmail: null,
  fulfilment: "DELIVERY",
  deliveryAddress: "12 Oxford St, Osu",
  deliveryNotes: null,
  notes: null,
  priced: {
    lines: [
      { productId: "classic-smash-burger", name: "Classic Smash Burger", sizeName: "Large", spice: null, extras: ["Bacon"], quantity: 2, unitPrice: 2049, lineTotal: 4098, instructions: null },
    ],
    subtotal: 4098,
    discount: 410,
    deliveryFee: 0,
    total: 3688,
    promoCode: "SAVE10",
  },
};

function respond(status: number, body: unknown) {
  return Promise.resolve(new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }));
}

describe("engine order repository", () => {
  let calls: { url: string; init: RequestInit }[];

  beforeEach(() => {
    calls = [];
    vi.stubEnv("BO_API_TOKEN", "t".repeat(40));
    vi.stubEnv("BO_ENGINE_URL", "http://127.0.0.1:9999");
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.unstubAllEnvs();
  });

  const fetchReturning = (status: number, body: unknown) =>
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string, init: RequestInit) => {
        calls.push({ url, init });
        return respond(status, body);
      }),
    );

  it("builds the engine order without nulls and with the expected total", () => {
    const doc = toEngineOrder(data, "BO-ABC123") as Record<string, unknown>;
    expect(doc).not.toHaveProperty("notes");
    expect(doc).not.toHaveProperty("customer_id");
    expect(doc.expected_total).toBe(3688);
    expect(doc.discount).toBe(410);
    expect(doc.promo_code).toBe("SAVE10");
    expect(doc.items).toEqual([{ sku: "classic-smash-burger", name: "Classic Smash Burger", qty: 2, unit_price: 2049, size: "Large", extras: ["Bacon"] }]);
    expect(doc.customer).toEqual({ name: "Ama Mensah", phone: "0240000000" });
  });

  it("maps engine statuses, treating a partial payment as unpaid", () => {
    const o = toCreated({ id: "1", reference: "BO-1", status: "accepted", payment_status: "partially_paid", total: 100 });
    expect(o).toEqual({ id: "1", reference: "BO-1", status: "CONFIRMED", paymentStatus: "UNPAID", total: 100 });
    expect(toCreated({ id: "1", reference: "r", status: "placed", payment_status: "overpaid", total: 1 }).paymentStatus).toBe("PAID");
  });

  it("sends the bearer token and creates the order", async () => {
    fetchReturning(201, { order: { id: "o1", reference: "BO-ABC123", status: "placed", payment_status: "unpaid", total: 3688 }, replayed: false });
    const created = await engineOrderRepository.createOrder(data, () => "BO-ABC123");
    expect(created).toEqual({ id: "o1", reference: "BO-ABC123", status: "PENDING", paymentStatus: "UNPAID", total: 3688 });
    expect(calls[0].url).toBe("http://127.0.0.1:9999/v1/orders");
    expect((calls[0].init.headers as Record<string, string>).Authorization).toBe(`Bearer ${"t".repeat(40)}`);
  });

  it("turns an exhausted promo into a pricing error (HTTP 409 for the customer)", async () => {
    fetchReturning(409, { error: "PROMO_EXHAUSTED", message: "This promo code has been fully used." });
    await expect(engineOrderRepository.createOrder(data, () => "BO-1")).rejects.toBeInstanceOf(PricingError);
  });

  it("keeps other engine refusals as engine errors", async () => {
    fetchReturning(409, { error: "TOTAL_MISMATCH", message: "no" });
    await expect(engineOrderRepository.createOrder(data, () => "BO-1")).rejects.toMatchObject({ code: "TOTAL_MISMATCH" });
  });

  it("returns null for an unknown idempotency key or promo", async () => {
    fetchReturning(404, { error: "NOT_FOUND" });
    expect(await engineOrderRepository.findByIdempotencyKey("key-unknown-1")).toBeNull();
    expect(await engineOrderRepository.findPromo("NOPE")).toBeNull();
  });

  it("maps catalogue products and promo rules", async () => {
    fetchReturning(200, { products: [{ _id: "p1", name: "P", base_price: 100, sizes: [{ name: "L", price_delta: 50 }], extras: [{ name: "E", price: 10, available: false }] }] });
    const cat = await engineOrderRepository.loadCatalogue(["p1"]);
    expect(cat.products.get("p1")).toEqual({ id: "p1", name: "P", basePrice: 100, promoPrice: null, available: true, sizes: [{ name: "L", priceDelta: 50 }], extras: [{ name: "E", price: 10, available: false }] });
    fetchReturning(200, { _id: "SAVE10", type: "PERCENT", value: 10, used_count: 3, ends_at: "2030-01-01T00:00:00Z" });
    const promo = await engineOrderRepository.findPromo("SAVE10");
    expect(promo).toMatchObject({ code: "SAVE10", usedCount: 3, maxUses: null, active: true, minSubtotal: 0 });
    expect(promo!.endsAt!.getUTCFullYear()).toBe(2030);
  });

  it("reports an unreachable engine clearly", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.reject(new TypeError("fetch failed"))));
    await expect(engineOrderRepository.findPromo("X")).rejects.toMatchObject({ code: "ENGINE_UNREACHABLE", status: 503 });
    await expect(engineOrderRepository.findPromo("X")).rejects.toBeInstanceOf(EngineError);
  });

  it("refuses to run without a token", async () => {
    vi.stubEnv("BO_API_TOKEN", "");
    await expect(engineOrderRepository.findPromo("X")).rejects.toThrow(/BO_API_TOKEN/);
  });
});

describe("store selection", () => {
  it("defaults to Prisma and rejects unknown stores", () => {
    expect(orderStore({} as NodeJS.ProcessEnv)).toBe("prisma");
    expect(() => orderStore({ ORDER_STORE: "mongo" } as unknown as NodeJS.ProcessEnv)).toThrow();
  });

  it("keeps the experimental engine out of production unless explicitly allowed", () => {
    expect(orderStore({ ORDER_STORE: "engine", NODE_ENV: "development" } as unknown as NodeJS.ProcessEnv)).toBe("engine");
    expect(() => orderStore({ ORDER_STORE: "engine", NODE_ENV: "production" } as unknown as NodeJS.ProcessEnv)).toThrow(/not allowed in production/);
    expect(orderStore({ ORDER_STORE: "engine", NODE_ENV: "production", ALLOW_EXPERIMENTAL_ENGINE: "yes" } as unknown as NodeJS.ProcessEnv)).toBe("engine");
  });
});

describe("engine menu", () => {
  it("groups by category in order and hides unavailable items", () => {
    const menu = groupEngineMenu([
      { _id: "b", name: "Zinger", category: "chicken", category_name: "Chicken", category_order: 3, base_price: 1 },
      { _id: "a", name: "Burger", category: "burgers", category_name: "Burgers", category_order: 1, base_price: 2, extras: [{ name: "x", price: 1, available: false }] },
      { _id: "c", name: "Gone", category: "burgers", base_price: 3, available: false },
    ]);
    expect(menu.categories.map((c) => c.slug)).toEqual(["burgers", "chicken"]);
    expect(menu.categories[0].products.map((p) => p.id)).toEqual(["a"]);
    expect(menu.categories[0].products[0].extras).toEqual([]);
  });
});
