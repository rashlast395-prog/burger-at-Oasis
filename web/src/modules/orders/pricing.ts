/**
 * Server-side pricing engine. The browser only sends *what* the customer wants
 * (product ids, options, quantities). Every price is looked up and calculated here.
 */

export type Fulfilment = "DELIVERY" | "COLLECTION" | "DINE_IN";

export interface CatalogueProduct {
  id: string;
  name: string;
  basePrice: number;
  promoPrice: number | null;
  available: boolean;
  sizes: { name: string; priceDelta: number }[];
  extras: { name: string; price: number; available: boolean }[];
}

export interface CatalogueSnapshot {
  products: Map<string, CatalogueProduct>;
}

export interface CartLineInput {
  productId: string;
  quantity: number;
  size?: string;
  spice?: string;
  extras?: string[];
  instructions?: string;
}

export interface PromoRule {
  code: string;
  type: "PERCENT" | "FIXED";
  value: number;
  minSubtotal: number;
  maxUses: number | null;
  usedCount: number;
  startsAt: Date | null;
  endsAt: Date | null;
  active: boolean;
}

export interface DeliveryConfig {
  /** Delivery is free when the subtotal reaches this amount (minor units). */
  freeOver: number;
  /** Flat fee charged below the free threshold (minor units). */
  fee: number;
}

export interface PricedLine {
  productId: string;
  name: string;
  sizeName: string | null;
  spice: string | null;
  extras: string[];
  quantity: number;
  unitPrice: number;
  lineTotal: number;
  instructions: string | null;
}

export interface PricedCart {
  lines: PricedLine[];
  subtotal: number;
  discount: number;
  deliveryFee: number;
  total: number;
  promoCode: string | null;
}

export type PricingErrorCode =
  | "EMPTY_CART"
  | "INVALID_QUANTITY"
  | "UNKNOWN_PRODUCT"
  | "PRODUCT_UNAVAILABLE"
  | "INVALID_SIZE"
  | "INVALID_EXTRA"
  | "PROMO_INVALID"
  | "PROMO_EXPIRED"
  | "PROMO_MIN_SUBTOTAL"
  | "PROMO_EXHAUSTED";

export class PricingError extends Error {
  constructor(
    public readonly code: PricingErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "PricingError";
  }
}

export const MAX_LINE_QUANTITY = 50;
export const MAX_LINES = 40;

export interface PriceCartOptions {
  catalogue: CatalogueSnapshot;
  lines: CartLineInput[];
  fulfilment: Fulfilment;
  delivery: DeliveryConfig;
  promo?: PromoRule | null;
  now?: Date;
}

export function priceCart(opts: PriceCartOptions): PricedCart {
  const { catalogue, lines, fulfilment, delivery, promo, now = new Date() } = opts;

  if (lines.length === 0 || lines.length > MAX_LINES) {
    throw new PricingError("EMPTY_CART", "Your cart is empty or has too many lines.");
  }

  const priced: PricedLine[] = lines.map((line) => {
    if (!Number.isInteger(line.quantity) || line.quantity < 1 || line.quantity > MAX_LINE_QUANTITY) {
      throw new PricingError("INVALID_QUANTITY", `Quantity must be between 1 and ${MAX_LINE_QUANTITY}.`);
    }
    const product = catalogue.products.get(line.productId);
    if (!product) throw new PricingError("UNKNOWN_PRODUCT", "A product in your cart no longer exists.");
    if (!product.available) {
      throw new PricingError("PRODUCT_UNAVAILABLE", `${product.name} is currently unavailable.`);
    }

    let sizeName: string | null = null;
    let sizeDelta = 0;
    if (product.sizes.length > 0) {
      const wanted = line.size ?? product.sizes[0].name;
      const size = product.sizes.find((s) => s.name === wanted);
      if (!size) throw new PricingError("INVALID_SIZE", `${wanted} is not a size for ${product.name}.`);
      sizeName = size.name;
      sizeDelta = size.priceDelta;
    } else if (line.size) {
      throw new PricingError("INVALID_SIZE", `${product.name} has no size options.`);
    }

    const wantedExtras = Array.from(new Set(line.extras ?? []));
    let extrasTotal = 0;
    for (const name of wantedExtras) {
      const extra = product.extras.find((e) => e.name === name);
      if (!extra || !extra.available) {
        throw new PricingError("INVALID_EXTRA", `${name} is not available for ${product.name}.`);
      }
      extrasTotal += extra.price;
    }

    const unitPrice = (product.promoPrice ?? product.basePrice) + sizeDelta + extrasTotal;
    return {
      productId: product.id,
      name: product.name,
      sizeName,
      spice: line.spice?.trim() || null,
      extras: wantedExtras,
      quantity: line.quantity,
      unitPrice,
      lineTotal: unitPrice * line.quantity,
      instructions: line.instructions?.trim().slice(0, 300) || null,
    };
  });

  const subtotal = priced.reduce((sum, l) => sum + l.lineTotal, 0);

  let discount = 0;
  let promoCode: string | null = null;
  if (promo) {
    discount = promoDiscount(promo, subtotal, now);
    promoCode = promo.code;
  }

  const deliveryFee =
    fulfilment === "DELIVERY" && subtotal < delivery.freeOver ? delivery.fee : 0;

  return {
    lines: priced,
    subtotal,
    discount,
    deliveryFee,
    total: subtotal - discount + deliveryFee,
    promoCode,
  };
}

export function promoDiscount(promo: PromoRule, subtotal: number, now: Date): number {
  if (!promo.active) throw new PricingError("PROMO_INVALID", "This promo code is not active.");
  if ((promo.startsAt && now < promo.startsAt) || (promo.endsAt && now > promo.endsAt)) {
    throw new PricingError("PROMO_EXPIRED", "This promo code has expired.");
  }
  if (promo.maxUses !== null && promo.usedCount >= promo.maxUses) {
    throw new PricingError("PROMO_EXHAUSTED", "This promo code has been fully used.");
  }
  if (subtotal < promo.minSubtotal) {
    throw new PricingError("PROMO_MIN_SUBTOTAL", "Your order is below the minimum for this code.");
  }
  const raw = promo.type === "PERCENT" ? Math.floor((subtotal * promo.value) / 100) : promo.value;
  return Math.min(Math.max(raw, 0), subtotal);
}
