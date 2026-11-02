/** Staff operations on orders. Currently backed by the engine only. */
import { engine } from "@/server/engine/client";
import { orderStore } from "@/server/repositories";

export class StoreUnsupported extends Error {
  constructor() {
    super("Staff tools currently need ORDER_STORE=engine; the PostgreSQL version is not built yet.");
    this.name = "StoreUnsupported";
  }
}

/** Order statuses as the engine names them, in the order a kitchen sees them. */
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

function ensureEngine() {
  if (orderStore() !== "engine") throw new StoreUnsupported();
}

export async function listOrders(): Promise<StaffOrder[]> {
  ensureEngine();
  const res = await engine<{ orders: StaffOrder[] }>("GET", "/orders");
  return (res?.orders ?? []).sort((a, b) => b.created_at - a.created_at);
}

export async function changeStatus(id: string, status: string): Promise<StaffOrder> {
  ensureEngine();
  if (!(BOARD_STATUSES as readonly string[]).includes(status)) throw new RangeError(`unknown status ${status}`);
  const res = await engine<{ order: StaffOrder }>("POST", `/orders/${encodeURIComponent(id)}/status`, { status });
  return res!.order;
}

export async function takeCash(id: string, amount: number) {
  ensureEngine();
  if (!Number.isInteger(amount) || amount <= 0) throw new RangeError("amount must be a positive whole number of pesewas");
  const res = await engine<{ payment: StaffOrder["payment"] & { total: number } }>("POST", `/orders/${encodeURIComponent(id)}/payments`, {
    method: "cash",
    amount,
  });
  return res!.payment;
}
