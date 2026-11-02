import type { OrderRepository } from "@/modules/orders/order.service";

export type OrderStore = "prisma" | "engine";

/**
 * Which store holds orders: `prisma` (PostgreSQL, the default) or `engine` (the custom
 * Burger Oasis engine via its local API). The engine is experimental, so it is refused in
 * production unless ALLOW_EXPERIMENTAL_ENGINE=yes is set deliberately.
 */
export function orderStore(env: NodeJS.ProcessEnv = process.env): OrderStore {
  const store = env.ORDER_STORE ?? "prisma";
  if (store !== "prisma" && store !== "engine") {
    throw new Error(`ORDER_STORE must be "prisma" or "engine", not "${store}".`);
  }
  if (store === "engine" && env.NODE_ENV === "production" && env.ALLOW_EXPERIMENTAL_ENGINE !== "yes") {
    throw new Error("The experimental engine store is not allowed in production (set ALLOW_EXPERIMENTAL_ENGINE=yes only after acceptance tests).");
  }
  return store;
}

/** Loaded lazily so the engine store never needs Prisma (or a database) installed. */
export async function getOrderRepository(): Promise<OrderRepository> {
  if (orderStore() === "engine") return (await import("./engine-order.repository")).engineOrderRepository;
  return (await import("./order.repository")).prismaOrderRepository;
}
