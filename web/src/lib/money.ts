/**
 * All money in the system is an integer number of minor units (pesewas for GHS).
 * Never use floating point for prices.
 */
export const CURRENCY = process.env.NEXT_PUBLIC_CURRENCY ?? "GHS";

export function formatMoney(minor: number, currency: string = CURRENCY): string {
  return new Intl.NumberFormat("en-GH", { style: "currency", currency }).format(minor / 100);
}

/** Convert a decimal amount typed by an admin (e.g. 14.99) to minor units. */
export function toMinor(amount: number): number {
  return Math.round(amount * 100);
}
