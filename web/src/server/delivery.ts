import type { DeliveryConfig } from "@/modules/orders/pricing";

/** Delivery pricing from the environment (minor units), shared by every order store. */
export function deliveryConfigFromEnv(): DeliveryConfig {
  return {
    freeOver: Number(process.env.DELIVERY_FREE_OVER_MINOR ?? 3000),
    fee: Number(process.env.DELIVERY_FEE_MINOR ?? 499),
  };
}
