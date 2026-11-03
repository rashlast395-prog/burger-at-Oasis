/** Staff operations on orders, for whichever store is active (engine or PostgreSQL). */
import { engine } from "@/server/engine/client";
import { orderStore } from "@/server/repositories";

/** Order statuses as the board names them, in the order a kitchen sees them. */
export const BOARD_STATUSES = ["placed", "accepted", "preparing", "ready", "out_for_delivery", "completed", "cancelled"] as const;
export type BoardStatus = (typeof BOARD_STATUSES)[number];

export interface StaffOrder {
  _id: string;
  reference: string;
  status: BoardStatus;
  created_at: number;
  fulfilment?: string;
  channel?: string;
  customer?: { name?: string; phone?: string };
  delivery_address?: string;
  delivery_notes?: string;
  notes?: string;
  items: { sku: string; name: string; qty: number; unit_price: number; size?: string; extras?: string[]; spice?: string; instructions?: string }[];
  subtotal: number;
  discount: number;
  delivery_fee: number;
  total: number;
  promo_code?: string;
  payment: { status: string; paid: number; refunded: number; net: number };
}

const prisma = () => import("./prisma-orders");

export async function listOrders(): Promise<StaffOrder[]> {
  if (orderStore() === "prisma") return (await prisma()).listOrders();
  const res = await engine<{ orders: StaffOrder[] }>("GET", "/orders");
  return (res?.orders ?? []).sort((a, b) => b.created_at - a.created_at);
}

export async function changeStatus(id: string, status: string): Promise<StaffOrder> {
  if (!(BOARD_STATUSES as readonly string[]).includes(status)) throw new RangeError(`unknown status ${status}`);
  if (orderStore() === "prisma") return (await prisma()).changeStatus(id, status as BoardStatus);
  const res = await engine<{ order: StaffOrder }>("POST", `/orders/${encodeURIComponent(id)}/status`, { status });
  return res!.order;
}

export async function takeCash(id: string, amount: number) {
  if (!Number.isInteger(amount) || amount <= 0) throw new RangeError("amount must be a positive whole number of pesewas");
  if (orderStore() === "prisma") return (await prisma()).takeCash(id, amount);
  const res = await engine<{ payment: StaffOrder["payment"] & { total: number } }>("POST", `/orders/${encodeURIComponent(id)}/payments`, {
    method: "cash",
    amount,
  });
  return res!.payment;
}
