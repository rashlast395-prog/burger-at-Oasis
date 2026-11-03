"use client";

/**
 * Staff order board: sign in, see orders as they arrive, move them through the kitchen and
 * record cash. The server enforces every rule (allowed transitions, payment limits); the
 * buttons here only offer the moves that can succeed.
 */
import { useCallback, useEffect, useState } from "react";
import { formatMoney } from "@/lib/money";
import type { StaffOrder } from "@/server/staff/orders";

const NEXT: Record<string, string[]> = {
  placed: ["accepted", "cancelled"],
  accepted: ["preparing", "cancelled"],
  preparing: ["ready", "cancelled"],
  ready: ["out_for_delivery", "completed", "cancelled"],
  out_for_delivery: ["completed", "cancelled"],
};

const LABEL: Record<string, string> = {
  placed: "New",
  accepted: "Accepted",
  preparing: "Preparing",
  ready: "Ready",
  out_for_delivery: "Out for delivery",
  completed: "Completed",
  cancelled: "Cancelled",
};

const ACTION: Record<string, string> = {
  accepted: "Accept",
  preparing: "Start preparing",
  ready: "Mark ready",
  out_for_delivery: "Send out",
  completed: "Complete",
  cancelled: "Cancel",
};

const TABS = [
  { id: "new", name: "New", statuses: ["placed"] },
  { id: "kitchen", name: "In the kitchen", statuses: ["accepted", "preparing"] },
  { id: "ready", name: "Ready / out", statuses: ["ready", "out_for_delivery"] },
  { id: "done", name: "Done", statuses: ["completed", "cancelled"] },
] as const;

export function StaffBoard() {
  const [orders, setOrders] = useState<StaffOrder[] | null>(null);
  const [signedIn, setSignedIn] = useState<boolean | null>(null);
  const [tab, setTab] = useState<(typeof TABS)[number]["id"]>("new");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    const res = await fetch("/api/staff/orders", { cache: "no-store" });
    if (res.status === 401) return setSignedIn(false);
    const d = await res.json().catch(() => ({}));
    if (!res.ok) return setError(d.message ?? "Orders could not be loaded.");
    setSignedIn(true);
    setError(null);
    setOrders(d.orders);
  }, []);

  useEffect(() => {
    // Poll for new orders; the first load runs as soon as the board is shown.
    const first = setTimeout(load, 0);
    const t = setInterval(load, 10_000);
    return () => {
      clearTimeout(first);
      clearInterval(t);
    };
  }, [load]);

  async function act(order: StaffOrder, url: string, body: unknown) {
    setBusy(order._id);
    const res = await fetch(url, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    const d = await res.json().catch(() => ({}));
    setBusy(null);
    if (res.status === 401) return setSignedIn(false);
    if (!res.ok) return setError(`${order.reference}: ${d.message ?? "that did not work"}`);
    await load();
  }

  if (signedIn === false) return <Login onDone={load} />;

  const shown = (orders ?? []).filter((o) => (TABS.find((t) => t.id === tab)!.statuses as readonly string[]).includes(o.status));
  const counts = Object.fromEntries(TABS.map((t) => [t.id, (orders ?? []).filter((o) => (t.statuses as readonly string[]).includes(o.status)).length]));

  return (
    <div className="flex min-h-screen flex-col">
      <header className="sticky top-0 z-10 border-b border-line bg-background/95 backdrop-blur">
        <div className="mx-auto flex max-w-6xl items-center justify-between px-4 py-3 sm:px-6">
          <span className="text-lg font-bold">
            Burger <span className="text-brand">Oasis</span> <span className="font-normal text-muted">· Orders</span>
          </span>
          <button
            onClick={async () => {
              await fetch("/api/staff/login", { method: "DELETE" });
              setSignedIn(false);
            }}
            className="rounded-full px-3 py-1.5 text-sm ring-1 ring-line hover:ring-brand"
          >
            Sign out
          </button>
        </div>
        <nav className="mx-auto flex max-w-6xl gap-2 overflow-x-auto px-4 pb-3 sm:px-6">
          {TABS.map((t) => (
            <button key={t.id} onClick={() => setTab(t.id)} className={`shrink-0 rounded-full px-4 py-1.5 text-sm font-medium ${tab === t.id ? "bg-brand text-brand-ink" : "bg-surface ring-1 ring-line"}`}>
              {t.name}
              {counts[t.id] ? ` (${counts[t.id]})` : ""}
            </button>
          ))}
        </nav>
      </header>
      <main className="mx-auto w-full max-w-6xl flex-1 px-4 py-6 sm:px-6">
        {error && (
          <p role="alert" className="mb-4 rounded-xl bg-danger/10 px-4 py-3 text-sm text-danger">
            {error}
          </p>
        )}
        {orders === null && <p className="text-muted">Loading orders…</p>}
        {orders && shown.length === 0 && <p className="text-muted">No orders here right now.</p>}
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {shown.map((o) => (
            <OrderCard key={o._id} order={o} busy={busy === o._id} onStatus={(s) => act(o, `/api/staff/orders/${o._id}/status`, { status: s })} onCash={(amount) => act(o, `/api/staff/orders/${o._id}/pay`, { amount })} />
          ))}
        </div>
      </main>
    </div>
  );
}

function OrderCard({ order: o, busy, onStatus, onCash }: { order: StaffOrder; busy: boolean; onStatus: (s: string) => void; onCash: (amount: number) => void }) {
  const due = o.total - o.payment.net;
  const paid = o.payment.status === "paid" || o.payment.status === "overpaid";
  const time = new Date(o.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return (
    <article className="flex flex-col gap-3 rounded-2xl bg-surface p-4 ring-1 ring-line">
      <div className="flex items-start justify-between gap-2">
        <div>
          <p className="font-semibold">{o.reference}</p>
          <p className="text-sm text-muted">
            {time} · {o.fulfilment === "DELIVERY" ? "Delivery" : o.fulfilment === "COLLECTION" ? "Collection" : o.fulfilment ?? ""} · {o.channel ?? ""}
          </p>
        </div>
        <span className="rounded-full bg-background px-2.5 py-1 text-xs font-medium ring-1 ring-line">{LABEL[o.status]}</span>
      </div>
      <ul className="space-y-1 text-sm">
        {o.items.map((it, i) => (
          <li key={i}>
            <span className="font-medium tabular-nums">{it.qty}×</span> {it.name}
            {(it.size || it.extras?.length) && <span className="text-muted"> · {[it.size, ...(it.extras ?? [])].filter(Boolean).join(", ")}</span>}
            {it.instructions && <span className="block pl-5 text-warn">“{it.instructions}”</span>}
          </li>
        ))}
      </ul>
      {(o.customer?.name || o.delivery_address || o.notes) && (
        <div className="text-sm">
          {o.customer?.name && (
            <p>
              {o.customer.name}
              {o.customer.phone && (
                <a href={`tel:${o.customer.phone}`} className="ml-2 text-brand underline">
                  {o.customer.phone}
                </a>
              )}
            </p>
          )}
          {o.delivery_address && <p className="text-muted">{o.delivery_address}</p>}
          {o.notes && <p className="text-warn">Note: {o.notes}</p>}
        </div>
      )}
      <div className="flex items-center justify-between border-t border-line pt-3 text-sm">
        <span className="font-semibold tabular-nums">{formatMoney(o.total)}</span>
        <span className={paid ? "text-ok" : "text-warn"}>{paid ? "Paid" : o.payment.net > 0 ? `${formatMoney(due)} still due` : "Not paid"}</span>
      </div>
      <div className="flex flex-wrap gap-2">
        {(NEXT[o.status] ?? []).map((s) => (
          <button
            key={s}
            disabled={busy}
            onClick={() => {
              if (s === "cancelled" && !confirm(`Cancel order ${o.reference}?`)) return;
              onStatus(s);
            }}
            className={`rounded-full px-3 py-1.5 text-sm font-medium disabled:opacity-50 ${s === "cancelled" ? "text-danger ring-1 ring-line" : "bg-brand text-brand-ink hover:bg-brand-strong"}`}
          >
            {ACTION[s]}
          </button>
        ))}
        {!paid && o.status !== "cancelled" && due > 0 && (
          <button disabled={busy} onClick={() => confirm(`Record ${formatMoney(due)} cash for ${o.reference}?`) && onCash(due)} className="rounded-full px-3 py-1.5 text-sm font-medium ring-1 ring-ok text-ok disabled:opacity-50">
            Cash received {formatMoney(due)}
          </button>
        )}
      </div>
    </article>
  );
}

function Login({ onDone }: { onDone: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  return (
    <main className="flex min-h-screen items-center justify-center px-4">
      <form
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          const res = await fetch("/api/staff/login", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ password }) });
          const d = await res.json().catch(() => ({}));
          setBusy(false);
          if (!res.ok) return setError(d.message ?? "Sign-in failed.");
          setPassword("");
          onDone();
        }}
        className="w-full max-w-sm space-y-4 rounded-2xl bg-surface p-6 ring-1 ring-line"
      >
        <h1 className="text-xl font-bold">
          Burger <span className="text-brand">Oasis</span> staff
        </h1>
        <label className="block text-sm">
          Staff password
          <input type="password" required value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" className="mt-1 w-full rounded-xl bg-background px-3 py-2 ring-1 ring-line outline-none focus:ring-brand" />
        </label>
        {error && <p role="alert" className="text-sm text-danger">{error}</p>}
        <button disabled={busy} className="w-full rounded-full bg-brand py-2.5 font-semibold text-brand-ink hover:bg-brand-strong disabled:opacity-60">
          {busy ? "Signing in…" : "Sign in"}
        </button>
      </form>
    </main>
  );
}
