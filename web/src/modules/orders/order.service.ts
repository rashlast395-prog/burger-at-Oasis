import { z } from "zod";
import {
  priceCart,
  PricingError,
  type CatalogueSnapshot,
  type DeliveryConfig,
  type PricedCart,
  type PromoRule,
} from "./pricing";

const phoneDigits = (s: string) => s.replace(/\D/g, "").length;

export const cartLineSchema = z.object({
  productId: z.string().min(1).max(64),
  quantity: z.number().int().min(1).max(50),
  size: z.string().max(40).optional(),
  spice: z.string().max(40).optional(),
  extras: z.array(z.string().max(60)).max(20).optional(),
  instructions: z.string().max(300).optional(),
});

export const quoteInputSchema = z.object({
  lines: z.array(cartLineSchema).min(1).max(40),
  fulfilment: z.enum(["DELIVERY", "COLLECTION", "DINE_IN"]),
  promoCode: z.string().trim().max(40).optional(),
});

export const createOrderInputSchema = quoteInputSchema
  .extend({
    idempotencyKey: z.string().min(8).max(100),
    customerName: z.string().trim().min(2).max(100),
    customerPhone: z.string().trim().refine((p) => phoneDigits(p) >= 8 && phoneDigits(p) <= 15, "Invalid phone number"),
    customerEmail: z.string().trim().email().max(120).optional().or(z.literal("")),
    deliveryAddress: z.string().trim().max(250).optional(),
    deliveryNotes: z.string().trim().max(300).optional(),
    notes: z.string().trim().max(500).optional(),
  })
  .superRefine((v, ctx) => {
    if (v.fulfilment === "DELIVERY" && (v.deliveryAddress ?? "").length < 5) {
      ctx.addIssue({ code: "custom", path: ["deliveryAddress"], message: "A delivery address is required." });
    }
  });

export type QuoteInput = z.infer<typeof quoteInputSchema>;
export type CreateOrderInput = z.infer<typeof createOrderInputSchema>;

export interface NewOrderData {
  idempotencyKey: string;
  branchId: string;
  channel: "WEB" | "POS" | "PHONE";
  customerId: string | null;
  customerName: string;
  customerPhone: string;
  customerEmail: string | null;
  fulfilment: CreateOrderInput["fulfilment"];
  deliveryAddress: string | null;
  deliveryNotes: string | null;
  notes: string | null;
  priced: PricedCart;
}

export interface CreatedOrder {
  id: string;
  reference: string;
  status: string;
  paymentStatus: string;
  total: number;
}

export interface OrderRepository {
  defaultBranchId(): Promise<string>;
  deliveryConfig(): Promise<DeliveryConfig>;
  loadCatalogue(productIds: string[]): Promise<CatalogueSnapshot>;
  findPromo(code: string): Promise<PromoRule | null>;
  findByIdempotencyKey(key: string): Promise<CreatedOrder | null>;
  /**
   * Must atomically create the order, its items, the first OrderEvent and
   * increment the promo usage. If the idempotency key already exists it must
   * return the existing order instead of failing.
   */
  createOrder(data: NewOrderData, newReference: () => string): Promise<CreatedOrder>;
}

export function makeReference(now: Date = new Date(), rand: () => number = Math.random): string {
  const t = now.getTime().toString(36).toUpperCase().slice(-5);
  const r = Math.floor(rand() * 36 ** 3)
    .toString(36)
    .toUpperCase()
    .padStart(3, "0");
  return `BO-${t}${r}`;
}

async function priceFromInput(repo: OrderRepository, input: QuoteInput, now: Date): Promise<PricedCart> {
  const ids = Array.from(new Set(input.lines.map((l) => l.productId)));
  const [catalogue, delivery] = await Promise.all([repo.loadCatalogue(ids), repo.deliveryConfig()]);
  const promo = input.promoCode ? await repo.findPromo(input.promoCode.toUpperCase()) : null;
  if (input.promoCode && !promo) throw new PricingError("PROMO_INVALID", "That promo code is not valid.");
  return priceCart({ catalogue, lines: input.lines, fulfilment: input.fulfilment, delivery, promo, now });
}

/** Price a cart without creating anything. Safe to call from the browser as the cart changes. */
export async function quoteOrder(repo: OrderRepository, rawInput: unknown, now = new Date()): Promise<PricedCart> {
  const input = quoteInputSchema.parse(rawInput);
  return priceFromInput(repo, input, now);
}

export async function placeOrder(
  repo: OrderRepository,
  rawInput: unknown,
  ctx: { customerId?: string | null; channel?: "WEB" | "POS" | "PHONE"; now?: Date } = {},
): Promise<{ order: CreatedOrder; priced: PricedCart | null; replayed: boolean }> {
  const input = createOrderInputSchema.parse(rawInput);
  const now = ctx.now ?? new Date();

  // Duplicate-submission protection: the same key always yields the same order.
  const existing = await repo.findByIdempotencyKey(input.idempotencyKey);
  if (existing) return { order: existing, priced: null, replayed: true };

  const priced = await priceFromInput(repo, input, now);
  const branchId = await repo.defaultBranchId();

  const order = await repo.createOrder(
    {
      idempotencyKey: input.idempotencyKey,
      branchId,
      channel: ctx.channel ?? "WEB",
      customerId: ctx.customerId ?? null,
      customerName: input.customerName,
      customerPhone: input.customerPhone,
      customerEmail: input.customerEmail || null,
      fulfilment: input.fulfilment,
      deliveryAddress: input.deliveryAddress || null,
      deliveryNotes: input.deliveryNotes || null,
      notes: input.notes || null,
      priced,
    },
    () => makeReference(now),
  );
  return { order, priced, replayed: false };
}
