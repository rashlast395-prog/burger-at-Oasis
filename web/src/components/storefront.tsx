"use client";

/**
 * Customer storefront: menu, item options, cart, checkout, confirmation.
 * Prices shown in the cart always come from the server's quote (`/api/orders/quote`); the
 * browser only says what the customer wants.
 */
import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { formatMoney } from "@/lib/money";

interface Product {
  id: string;
  name: string;
  description: string | null;
  basePrice: number;
  promoPrice: number | null;
  imageUrl: string | null;
  sizes: { name: string; priceDelta: number }[];
  extras: { name: string; price: number }[];
}
interface Category {
  slug: string;
  name: string;
  products: Product[];
}
interface CartLine {
  key: string;
  productId: string;
  name: string;
  quantity: number;
  size?: string;
  extras: string[];
  instructions?: string;
}
interface Quote {
  lines: { productId: string; unitPrice: number; lineTotal: number }[];
  subtotal: number;
  discount: number;
  deliveryFee: number;
  total: number;
  promoCode: string | null;
}
interface Placed {
  reference: string;
  total: number;
  fulfilment: "DELIVERY" | "COLLECTION";
}

const CART_KEY = "bo-cart-v1";
const price = (p: Product) => p.promoPrice ?? p.basePrice;

function loadCart(): CartLine[] {
  try {
    const raw = localStorage.getItem(CART_KEY);
    return raw ? (JSON.parse(raw) as CartLine[]) : [];
  } catch {
    return [];
  }
}

function saveCart(cart: CartLine[]) {
  try {
    localStorage.setItem(CART_KEY, JSON.stringify(cart));
  } catch {
    /* private mode: the cart simply is not remembered */
  }
}

async function post<T>(url: string, body: unknown): Promise<{ ok: boolean; data: T & { error?: string; message?: string; issues?: { message: string }[] } }> {
  const res = await fetch(url, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const data = await res.json().catch(() => ({ error: "NETWORK", message: "Could not reach the restaurant. Check your connection." }));
  return { ok: res.ok, data };
}

const errorText = (d: { message?: string; issues?: { message: string }[] }) => d.issues?.map((i) => i.message).join(" ") || d.message || "Something went wrong.";

export function Storefront() {
  const [menu, setMenu] = useState<Category[] | null>(null);
  const [menuError, setMenuError] = useState<string | null>(null);
  const [active, setActive] = useState<string>("");
  const [cart, setCart] = useState<CartLine[]>([]);
  const [picking, setPicking] = useState<Product | null>(null);
  const [cartOpen, setCartOpen] = useState(false);
  const [placed, setPlaced] = useState<Placed | null>(null);

  useEffect(() => {
    // The saved cart lives in localStorage, which only exists in the browser (not during
    // server rendering), so it is restored after the first render.
    const restore = setTimeout(() => setCart(loadCart()), 0);
    fetch("/api/menu")
      .then(async (r) => {
        const d = await r.json();
        if (!r.ok) throw new Error(d.message ?? "The menu could not be loaded.");
        setMenu(d.categories);
        setActive(d.categories[0]?.slug ?? "");
      })
      .catch((e: Error) => setMenuError(e.message));
    return () => clearTimeout(restore);
  }, []);

  const updateCart = useCallback((next: CartLine[]) => {
    setCart(next);
    saveCart(next);
  }, []);

  const count = cart.reduce((n, l) => n + l.quantity, 0);

  if (placed) {
    return (
      <Shell count={0} onCart={() => {}}>
        <Confirmation placed={placed} onDone={() => setPlaced(null)} />
      </Shell>
    );
  }

  return (
    <Shell count={count} onCart={() => setCartOpen(true)}>
      <section className="mx-auto max-w-6xl px-4 pt-8 pb-4 sm:px-6">
        <h1 className="text-3xl font-bold tracking-tight sm:text-4xl">Hungry? We&apos;ve got you.</h1>
        <p className="mt-2 max-w-xl text-muted">Order for delivery or collection. Pay with cash when your food arrives.</p>
      </section>

      {menuError && <p className="mx-auto max-w-6xl px-4 py-6 text-danger sm:px-6">{menuError}</p>}
      {!menu && !menuError && <p className="mx-auto max-w-6xl px-4 py-6 text-muted sm:px-6">Loading the menu…</p>}

      {menu && (
        <>
          <nav className="sticky top-[57px] z-10 border-b border-line bg-background/95 backdrop-blur">
            <div className="mx-auto flex max-w-6xl gap-2 overflow-x-auto px-4 py-3 sm:px-6">
              {menu.map((c) => (
                <button
                  key={c.slug}
                  onClick={() => {
                    setActive(c.slug);
                    document.getElementById(`cat-${c.slug}`)?.scrollIntoView({ behavior: "smooth", block: "start" });
                  }}
                  className={`shrink-0 rounded-full px-4 py-1.5 text-sm font-medium transition ${
                    active === c.slug ? "bg-brand text-brand-ink" : "bg-surface text-foreground ring-1 ring-line hover:ring-brand"
                  }`}
                >
                  {c.name}
                </button>
              ))}
            </div>
          </nav>
          <main className="mx-auto w-full max-w-6xl flex-1 px-4 pb-28 sm:px-6">
            {menu.map((c) => (
              <section key={c.slug} id={`cat-${c.slug}`} className="scroll-mt-32 pt-8">
                <h2 className="mb-4 text-xl font-semibold">{c.name}</h2>
                <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
                  {c.products.map((p) => (
                    <ProductCard key={p.id} product={p} onAdd={() => setPicking(p)} />
                  ))}
                </div>
              </section>
            ))}
          </main>
        </>
      )}

      {count > 0 && !cartOpen && (
        <button
          onClick={() => setCartOpen(true)}
          className="fixed inset-x-4 bottom-4 z-20 mx-auto flex max-w-md items-center justify-between rounded-2xl bg-brand px-5 py-4 font-semibold text-brand-ink shadow-lg sm:inset-x-auto sm:right-6"
        >
          <span>View cart ({count})</span>
          <span aria-hidden>→</span>
        </button>
      )}

      {picking && (
        <OptionsDialog
          product={picking}
          onClose={() => setPicking(null)}
          onAdd={(line) => {
            updateCart([...cart, line]);
            setPicking(null);
          }}
        />
      )}
      {cartOpen && <CartPanel cart={cart} setCart={updateCart} onClose={() => setCartOpen(false)} onPlaced={(p) => { updateCart([]); setCartOpen(false); setPlaced(p); }} />}
    </Shell>
  );
}

function Shell({ count, onCart, children }: { count: number; onCart: () => void; children: React.ReactNode }) {
  return (
    <div className="flex min-h-screen flex-col">
      <header className="sticky top-0 z-20 border-b border-line bg-background/95 backdrop-blur">
        <div className="mx-auto flex max-w-6xl items-center justify-between px-4 py-3 sm:px-6">
          <Link href="/" className="text-lg font-bold">
            Burger <span className="text-brand">Oasis</span>
          </Link>
          <button onClick={onCart} className="rounded-full px-3 py-1.5 text-sm font-medium ring-1 ring-line hover:ring-brand" aria-label={`Cart, ${count} items`}>
            Cart{count > 0 ? ` · ${count}` : ""}
          </button>
        </div>
      </header>
      {children}
    </div>
  );
}

function ProductCard({ product: p, onAdd }: { product: Product; onAdd: () => void }) {
  return (
    <article className="flex flex-col overflow-hidden rounded-2xl bg-surface ring-1 ring-line">
      {p.imageUrl && (
        // eslint-disable-next-line @next/next/no-img-element
        <img src={p.imageUrl} alt="" className="aspect-[4/3] w-full object-cover" loading="lazy" />
      )}
      <div className="flex flex-1 flex-col gap-2 p-4">
        <div className="flex items-start justify-between gap-3">
          <h3 className="font-semibold">{p.name}</h3>
          <span className="shrink-0 font-semibold tabular-nums">
            {p.promoPrice != null && <s className="mr-1 text-sm font-normal text-muted">{formatMoney(p.basePrice)}</s>}
            {formatMoney(price(p))}
          </span>
        </div>
        {p.description && <p className="text-sm text-muted">{p.description}</p>}
        <button onClick={onAdd} className="mt-auto self-start rounded-full bg-brand px-4 py-2 text-sm font-semibold text-brand-ink hover:bg-brand-strong">
          Add
        </button>
      </div>
    </article>
  );
}

function OptionsDialog({ product: p, onClose, onAdd }: { product: Product; onClose: () => void; onAdd: (l: CartLine) => void }) {
  const [size, setSize] = useState(p.sizes[0]?.name);
  const [extras, setExtras] = useState<string[]>([]);
  const [qty, setQty] = useState(1);
  const [note, setNote] = useState("");
  // Display only; the server reprices everything at checkout.
  const unit = price(p) + (p.sizes.find((s) => s.name === size)?.priceDelta ?? 0) + p.extras.filter((e) => extras.includes(e.name)).reduce((s, e) => s + e.price, 0);

  return (
    <div className="fixed inset-0 z-30 flex items-end justify-center bg-black/50 sm:items-center" onClick={onClose}>
      <div role="dialog" aria-modal="true" aria-label={p.name} className="max-h-[90vh] w-full max-w-md overflow-y-auto rounded-t-2xl bg-surface p-5 sm:rounded-2xl" onClick={(e) => e.stopPropagation()}>
        <div className="flex items-start justify-between">
          <h3 className="text-lg font-semibold">{p.name}</h3>
          <button onClick={onClose} aria-label="Close" className="text-muted hover:text-foreground">✕</button>
        </div>
        {p.sizes.length > 0 && (
          <fieldset className="mt-4">
            <legend className="mb-2 text-sm font-medium">Size</legend>
            <div className="grid gap-2">
              {p.sizes.map((s) => (
                <label key={s.name} className={`flex cursor-pointer items-center justify-between rounded-xl px-3 py-2 ring-1 ${size === s.name ? "ring-brand" : "ring-line"}`}>
                  <span className="flex items-center gap-2">
                    <input type="radio" name="size" checked={size === s.name} onChange={() => setSize(s.name)} className="accent-[var(--brand)]" />
                    {s.name}
                  </span>
                  <span className="text-sm text-muted">{s.priceDelta ? `+${formatMoney(s.priceDelta)}` : ""}</span>
                </label>
              ))}
            </div>
          </fieldset>
        )}
        {p.extras.length > 0 && (
          <fieldset className="mt-4">
            <legend className="mb-2 text-sm font-medium">Extras</legend>
            <div className="grid gap-2">
              {p.extras.map((e) => (
                <label key={e.name} className={`flex cursor-pointer items-center justify-between rounded-xl px-3 py-2 ring-1 ${extras.includes(e.name) ? "ring-brand" : "ring-line"}`}>
                  <span className="flex items-center gap-2">
                    <input
                      type="checkbox"
                      checked={extras.includes(e.name)}
                      onChange={() => setExtras(extras.includes(e.name) ? extras.filter((x) => x !== e.name) : [...extras, e.name])}
                      className="accent-[var(--brand)]"
                    />
                    {e.name}
                  </span>
                  <span className="text-sm text-muted">+{formatMoney(e.price)}</span>
                </label>
              ))}
            </div>
          </fieldset>
        )}
        <label className="mt-4 block text-sm font-medium">
          Special instructions
          <input value={note} onChange={(e) => setNote(e.target.value.slice(0, 300))} placeholder="e.g. no onions" className="mt-1 w-full rounded-xl bg-background px-3 py-2 ring-1 ring-line outline-none focus:ring-brand" />
        </label>
        <div className="mt-5 flex items-center gap-3">
          <div className="flex items-center rounded-full ring-1 ring-line">
            <button onClick={() => setQty(Math.max(1, qty - 1))} className="px-3 py-2" aria-label="Fewer">−</button>
            <span className="w-6 text-center tabular-nums">{qty}</span>
            <button onClick={() => setQty(Math.min(50, qty + 1))} className="px-3 py-2" aria-label="More">+</button>
          </div>
          <button
            onClick={() => onAdd({ key: crypto.randomUUID(), productId: p.id, name: p.name, quantity: qty, size, extras, instructions: note.trim() || undefined })}
            className="flex-1 rounded-full bg-brand px-4 py-3 font-semibold text-brand-ink hover:bg-brand-strong"
          >
            Add · {formatMoney(unit * qty)}
          </button>
        </div>
      </div>
    </div>
  );
}

function CartPanel({ cart, setCart, onClose, onPlaced }: { cart: CartLine[]; setCart: (c: CartLine[]) => void; onClose: () => void; onPlaced: (p: Placed) => void }) {
  const [fulfilment, setFulfilment] = useState<"DELIVERY" | "COLLECTION">("DELIVERY");
  const [promo, setPromo] = useState("");
  const [appliedPromo, setAppliedPromo] = useState<string | undefined>();
  const [quote, setQuote] = useState<Quote | null>(null);
  const [quoteError, setQuoteError] = useState<string | null>(null);
  const [form, setForm] = useState({ name: "", phone: "", email: "", address: "", notes: "" });
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  // One key per checkout attempt: pressing "Place order" twice cannot create two orders.
  const idempotencyKey = useRef(crypto.randomUUID());

  const lines = useMemo(
    () => cart.map((l) => ({ productId: l.productId, quantity: l.quantity, size: l.size, extras: l.extras, instructions: l.instructions })),
    [cart],
  );

  useEffect(() => {
    if (lines.length === 0) return; // nothing to price; the panel shows "empty" instead
    let cancelled = false;
    const t = setTimeout(async () => {
      const { ok, data } = await post<Quote>("/api/orders/quote", { lines, fulfilment, promoCode: appliedPromo });
      if (cancelled) return;
      if (ok) {
        setQuote(data);
        setQuoteError(null);
      } else {
        setQuoteError(errorText(data));
        if (appliedPromo && data.error?.startsWith("PROMO")) setAppliedPromo(undefined);
      }
    }, 200);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [lines, fulfilment, appliedPromo]);

  const setQty = (key: string, q: number) => setCart(q <= 0 ? cart.filter((l) => l.key !== key) : cart.map((l) => (l.key === key ? { ...l, quantity: Math.min(50, q) } : l)));

  async function placeOrder(e: React.FormEvent) {
    e.preventDefault();
    setSubmitting(true);
    setSubmitError(null);
    const { ok, data } = await post<{ order: { reference: string; total: number } }>("/api/orders", {
      lines,
      fulfilment,
      promoCode: appliedPromo,
      idempotencyKey: idempotencyKey.current,
      customerName: form.name,
      customerPhone: form.phone,
      customerEmail: form.email,
      deliveryAddress: fulfilment === "DELIVERY" ? form.address : undefined,
      notes: form.notes || undefined,
    });
    setSubmitting(false);
    if (!ok) return setSubmitError(errorText(data));
    idempotencyKey.current = crypto.randomUUID();
    onPlaced({ reference: data.order.reference, total: data.order.total, fulfilment });
  }

  const field = "mt-1 w-full rounded-xl bg-background px-3 py-2 ring-1 ring-line outline-none focus:ring-brand";

  return (
    <div className="fixed inset-0 z-30 flex justify-end bg-black/50" onClick={onClose}>
      <aside role="dialog" aria-modal="true" aria-label="Your order" className="flex h-full w-full max-w-md flex-col bg-surface" onClick={(e) => e.stopPropagation()}>
        <div className="flex items-center justify-between border-b border-line px-5 py-4">
          <h2 className="text-lg font-semibold">Your order</h2>
          <button onClick={onClose} aria-label="Close" className="text-muted hover:text-foreground">✕</button>
        </div>
        {cart.length === 0 ? (
          <p className="p-5 text-muted">Your cart is empty.</p>
        ) : (
          <form onSubmit={placeOrder} className="flex-1 overflow-y-auto px-5 pb-6">
            <ul className="divide-y divide-line">
              {cart.map((l, i) => (
                <li key={l.key} className="flex items-start justify-between gap-3 py-3">
                  <div>
                    <p className="font-medium">{l.name}</p>
                    <p className="text-sm text-muted">{[l.size, ...l.extras].filter(Boolean).join(" · ")}{l.instructions ? ` · “${l.instructions}”` : ""}</p>
                    <div className="mt-2 flex items-center gap-2 text-sm">
                      <button type="button" onClick={() => setQty(l.key, l.quantity - 1)} className="h-7 w-7 rounded-full ring-1 ring-line" aria-label={`One fewer ${l.name}`}>−</button>
                      <span className="w-5 text-center tabular-nums">{l.quantity}</span>
                      <button type="button" onClick={() => setQty(l.key, l.quantity + 1)} className="h-7 w-7 rounded-full ring-1 ring-line" aria-label={`One more ${l.name}`}>+</button>
                    </div>
                  </div>
                  <span className="tabular-nums">{quote?.lines[i] ? formatMoney(quote.lines[i].lineTotal) : "…"}</span>
                </li>
              ))}
            </ul>

            <div className="mt-4 grid grid-cols-2 gap-2 rounded-xl bg-background p-1 ring-1 ring-line">
              {(["DELIVERY", "COLLECTION"] as const).map((f) => (
                <button key={f} type="button" onClick={() => setFulfilment(f)} className={`rounded-lg py-2 text-sm font-medium ${fulfilment === f ? "bg-brand text-brand-ink" : ""}`}>
                  {f === "DELIVERY" ? "Delivery" : "Collection"}
                </button>
              ))}
            </div>

            <div className="mt-4 flex gap-2">
              <input value={promo} onChange={(e) => setPromo(e.target.value.toUpperCase())} placeholder="Promo code" className={`${field} mt-0`} aria-label="Promo code" />
              <button type="button" onClick={() => setAppliedPromo(promo.trim() || undefined)} className="shrink-0 rounded-xl px-4 text-sm font-medium ring-1 ring-line hover:ring-brand">
                Apply
              </button>
            </div>

            <dl className="mt-4 space-y-1 text-sm">
              <Row label="Subtotal" value={quote?.subtotal} />
              {!!quote?.discount && <Row label={`Discount (${quote.promoCode})`} value={-quote.discount} />}
              <Row label="Delivery" value={quote?.deliveryFee} free={fulfilment === "DELIVERY"} />
              <div className="flex justify-between border-t border-line pt-2 text-base font-semibold">
                <dt>Total</dt>
                <dd className="tabular-nums">{quote ? formatMoney(quote.total) : "…"}</dd>
              </div>
            </dl>
            {quoteError && <p className="mt-2 text-sm text-danger">{quoteError}</p>}

            <fieldset className="mt-6 space-y-3">
              <legend className="mb-1 font-semibold">Your details</legend>
              <label className="block text-sm">Name<input required minLength={2} value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} className={field} autoComplete="name" /></label>
              <label className="block text-sm">Phone<input required type="tel" value={form.phone} onChange={(e) => setForm({ ...form, phone: e.target.value })} className={field} autoComplete="tel" placeholder="024 000 0000" /></label>
              <label className="block text-sm">Email (optional)<input type="email" value={form.email} onChange={(e) => setForm({ ...form, email: e.target.value })} className={field} autoComplete="email" /></label>
              {fulfilment === "DELIVERY" && (
                <label className="block text-sm">Delivery address<input required minLength={5} value={form.address} onChange={(e) => setForm({ ...form, address: e.target.value })} className={field} autoComplete="street-address" /></label>
              )}
              <label className="block text-sm">Notes for the restaurant (optional)<input value={form.notes} onChange={(e) => setForm({ ...form, notes: e.target.value.slice(0, 500) })} className={field} /></label>
            </fieldset>

            <p className="mt-4 rounded-xl bg-background p-3 text-sm text-muted ring-1 ring-line">
              Pay with cash on {fulfilment === "DELIVERY" ? "delivery" : "collection"}. Mobile money and card payment are coming soon.
            </p>
            {submitError && <p className="mt-3 text-sm text-danger" role="alert">{submitError}</p>}
            <button type="submit" disabled={submitting || !quote} className="mt-4 w-full rounded-full bg-brand py-3 font-semibold text-brand-ink hover:bg-brand-strong disabled:opacity-60">
              {submitting ? "Placing your order…" : `Place order${quote ? ` · ${formatMoney(quote.total)}` : ""}`}
            </button>
          </form>
        )}
      </aside>
    </div>
  );
}

function Row({ label, value, free }: { label: string; value?: number; free?: boolean }) {
  return (
    <div className="flex justify-between">
      <dt className="text-muted">{label}</dt>
      <dd className="tabular-nums">{value === undefined ? "…" : value === 0 && free ? "Free" : formatMoney(value)}</dd>
    </div>
  );
}

function Confirmation({ placed, onDone }: { placed: Placed; onDone: () => void }) {
  return (
    <main className="mx-auto flex w-full max-w-md flex-1 flex-col items-center justify-center gap-4 px-6 py-16 text-center">
      <div className="flex h-14 w-14 items-center justify-center rounded-full bg-ok/15 text-2xl text-ok" aria-hidden>✓</div>
      <h1 className="text-2xl font-bold">Order received</h1>
      <p className="text-muted">
        Your order number is <strong className="text-foreground">{placed.reference}</strong>. Please have{" "}
        <strong className="text-foreground">{formatMoney(placed.total)}</strong> in cash ready on {placed.fulfilment === "DELIVERY" ? "delivery" : "collection"}.
      </p>
      <button onClick={onDone} className="mt-2 rounded-full bg-brand px-5 py-2.5 font-semibold text-brand-ink hover:bg-brand-strong">
        Order something else
      </button>
    </main>
  );
}
