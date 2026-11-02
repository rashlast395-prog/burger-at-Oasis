/**
 * Staff order operations on PostgreSQL (Prisma). Returns the same shape as the engine
 * version so the order board works with either store.
 */
import { Prisma } from "@prisma/client";
import { canTransition, type OrderStatus } from "@/modules/orders/status";
import { db } from "@/server/db";
import type { BoardStatus, StaffOrder } from "./orders";

const TO_BOARD: Record<OrderStatus, BoardStatus> = {
  PENDING: "placed",
  CONFIRMED: "accepted",
  PREPARING: "preparing",
  READY: "ready",
  OUT_FOR_DELIVERY: "out_for_delivery",
  COMPLETED: "completed",
  CANCELLED: "cancelled",
};
const FROM_BOARD = Object.fromEntries(Object.entries(TO_BOARD).map(([k, v]) => [v, k])) as Record<BoardStatus, OrderStatus>;

export class StaffConflict extends Error {
  constructor(message: string) {
    super(message);
    this.name = "StaffConflict";
  }
}

const withDetail = { items: true, payments: true } satisfies Prisma.OrderInclude;
type FullOrder = Prisma.OrderGetPayload<{ include: typeof withDetail }>;

/** Money actually received: payments that are PAID (refunds are not recorded yet). */
function paidSum(o: { payments: { amount: number; status: string }[] }) {
  return o.payments.filter((p) => p.status === "PAID").reduce((s, p) => s + p.amount, 0);
}

function summary(total: number, paid: number) {
  const status = paid <= 0 ? "unpaid" : paid < total ? "partially_paid" : paid === total ? "paid" : "overpaid";
  return { status, paid, refunded: 0, net: paid };
}

export function toStaffOrder(o: FullOrder): StaffOrder {
  return {
    _id: o.id,
    reference: o.reference,
    status: TO_BOARD[o.status as OrderStatus],
    created_at: o.createdAt.getTime(),
    fulfilment: o.fulfilment,
    channel: o.channel,
    customer: { name: o.customerName, phone: o.customerPhone },
    delivery_address: o.deliveryAddress ?? undefined,
    delivery_notes: o.deliveryNotes ?? undefined,
    notes: o.notes ?? undefined,
    items: o.items.map((i) => ({
      sku: i.productId,
      name: i.name,
      qty: i.quantity,
      unit_price: i.unitPrice,
      size: i.sizeName ?? undefined,
      extras: i.extras,
      spice: i.spice ?? undefined,
      instructions: i.instructions ?? undefined,
    })),
    subtotal: o.subtotal,
    discount: o.discount,
    delivery_fee: o.deliveryFee,
    total: o.total,
    promo_code: o.promoCode ?? undefined,
    payment: summary(o.total, paidSum(o)),
  };
}

export async function listOrders(): Promise<StaffOrder[]> {
  const rows = await db.order.findMany({ include: withDetail, orderBy: { createdAt: "desc" }, take: 500 });
  return rows.map(toStaffOrder);
}

export async function changeStatus(id: string, board: BoardStatus): Promise<StaffOrder> {
  const to = FROM_BOARD[board];
  return db.$transaction(async (tx) => {
    const order = await tx.order.findUnique({ where: { id } });
    if (!order) throw new StaffConflict("That order no longer exists.");
    const from = order.status as OrderStatus;
    if (!canTransition(from, to)) throw new StaffConflict(`An order cannot go from ${TO_BOARD[from]} to ${board}.`);
    // Only applies if nobody changed the order since we read it (two tills pressing at once).
    const res = await tx.order.updateMany({ where: { id, status: from }, data: { status: to } });
    if (res.count === 0) throw new StaffConflict("Someone else just changed this order. Refresh and try again.");
    await tx.orderEvent.create({ data: { orderId: id, type: "STATUS_CHANGED", fromStatus: from, toStatus: to } });
    return toStaffOrder(await tx.order.findUniqueOrThrow({ where: { id }, include: withDetail }));
  });
}

export async function takeCash(id: string, amount: number) {
  return db.$transaction(async (tx) => {
    const order = await tx.order.findUnique({ where: { id }, include: { payments: true } });
    if (!order) throw new StaffConflict("That order no longer exists.");
    if (order.status === "CANCELLED") throw new StaffConflict("This order is cancelled; do not take payment for it.");
    await tx.payment.create({ data: { orderId: id, method: "CASH", amount, status: "PAID", verifiedAt: new Date() } });
    const paid = paidSum(order) + amount;
    // A partly paid order stays UNPAID so it is never treated as settled.
    await tx.order.update({ where: { id }, data: { paymentStatus: paid >= order.total ? "PAID" : "UNPAID" } });
    await tx.orderEvent.create({ data: { orderId: id, type: "PAYMENT_RECORDED", data: { method: "CASH", amount } } });
    return { ...summary(order.total, paid), total: order.total };
  });
}
