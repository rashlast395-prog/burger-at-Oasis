# Burger Oasis Database Platform: Phase 1 (Audit, Architecture, Plan)

Status: **Phase 1 only. No engine code exists yet.** Nothing in this document is implemented unless marked "exists today".
Date: 2026-11-02.

---

## 1. Technical audit of the existing project

### 1.1 What exists today

| Part | Location | Technology | State |
|---|---|---|---|
| Live storefront | `Burger/sarab/` | Static HTML, CSS, vanilla JS (jQuery 3.7, Bootstrap 5, AOS, Swiper, Magnific Popup) | Live at burgeroasis.vercel.app |
| Shop layer | `Burger/sarab/js/shop.js`, `css/shop.css` | Vanilla JS, browser `localStorage` | Works in-browser: accounts (PBKDF2), wishlist, validated checkout, orders, reorder, receipts |
| Hosting | `vercel.json` | Vercel static output from `Burger/sarab` | Auto-deploys from `main` |
| Stage 2 platform (not live) | `web/` on branch `stage2-nextjs` | Next.js 16, TypeScript strict, Prisma schema (PostgreSQL), Vitest | Pricing, order service, status machine tested (35 tests). API **never run against a database** |

### 1.2 Functionality inventory (current live site)

Working in the browser: catalogue of 6 placeholder products, product popup with size/spice/extras, cart with persistence, promo codes (4), free-delivery threshold, checkout with validation, 4 payment *forms* (none take real payment), order history and cancellation, receipts, wishlist, saved addresses, reviews, search, sort, reservation and contact forms (via Formspree), keyword chatbot, cookie notice, legal pages.

### 1.3 What is NOT real today

- There is **no backend and no shared database**. Orders, accounts and stock exist only in each visitor's browser. The restaurant cannot see any order.
- **No real payment processing.** Card, Mobile Money and bank forms are demonstration only.
- **Prices are enforced in the browser**, so a customer could alter them.
- Order status is a timer simulation.
- No POS, kitchen display, admin dashboard or inventory.
- Branding and business details still contain placeholders (address, hours, social links, About/History/chefs/testimonials).

### 1.4 Integrations

Formspree (contact/reservation emails) only. No database, no payment gateway, no auth provider.

---

## 2. Environment and dependency report

| Item | Finding |
|---|---|
| OS | Windows 11 Pro |
| RAM / disk | 7.8 GB RAM, ~143 GB free |
| Node | v24.20 (OK) |
| Git | 2.55 (OK) |
| **Rust** | **Not installed** |
| **C/C++ build tools** | **Not installed** (the `link.exe` found is Git's coreutils, not the MSVC linker) |
| Python | Not installed |

To build the Rust engine on this laptop you must install: (1) `rustup` (stable toolchain), and (2) either Visual Studio Build Tools with the "Desktop development with C++" workload (about 3-6 GB, standard for the MSVC target) or the GNU target with MSYS2/MinGW. For the remote Linux node: a Linux VPS (2 vCPU / 2-4 GB RAM / SSD is a sensible start), a domain, and TLS (Caddy or Cloudflare).

---

## 3. Feasibility and trade-offs (needs your approval)

This section is deliberately direct, because the spec asks for ownership of technical decisions and for trade-offs to be explained.

**Building a database engine is a different kind of project from building a restaurant system.** A trustworthy engine needs crash-safe storage, a write-ahead log, B+ tree indexes, concurrency control, recovery and replication, each proven by adversarial testing (including injected crashes and torn writes). Mature engines (SQLite, PostgreSQL) took years of testing to reach their reliability. A realistic estimate for a **single-node** engine that I would be willing to trust with sales data is **several months of focused engineering**, and the distributed sync layer adds substantially more. This is not something I can honestly complete in a handful of sessions.

**What a bug here costs:** lost or duplicated sales, wrong stock, mismatched payments. The spec itself says (rule 13) that experimental engine code must not handle real transactions, and (rule 14) not to pass failed reliability tests. Taken together: **until the engine passes its acceptance gates, the restaurant needs some other way to store real orders**, or it keeps taking orders the manual way.

**Two requirements are in tension:** rule 1 forbids any existing database as the underlying storage, yet the business needs real, durable order storage soon. I will not secretly use one. Three honest options:

| Option | What it means | Risk | Time to a usable restaurant system |
|---|---|---|---|
| **A. Follow the spec strictly** | Build the custom engine first. The website stays a demo (no real orders) until the engine passes every gate | Long wait; real operations can't start | Many months |
| **B. Engine as a parallel project** (recommended) | Put all business logic behind a `Store` interface. Run the restaurant on a proven database now (SQLite locally for POS offline, a hosted server database online). Build the custom engine separately, and swap it in only after it passes the acceptance tests. Needs your explicit approval because it uses an existing engine as an *interim* store | Moderate; the interim store is replaced later by a controlled migration | Weeks for core selling features |
| **C. Drop the custom engine** | Use SQLite + a server database with a standard sync approach. Same offline-first POS, far less risk | Low | Weeks |

My recommendation is **B** if the custom engine is a strategic goal (a learning project, IP, or independence), and **C** if the goal is simply a reliable restaurant system. Either way the offline-first, event-sourced sync design below applies, because it is independent of which storage engine sits underneath.

---

## 4. Proposed architecture (valid under A, B or C)

### 4.1 Layers

```
 Website / POS / Admin / Kitchen / Console      (React + TypeScript; browsers never touch storage)
                     |
              Business API (TypeScript)          auth, permissions, pricing, orders, payments, stock, reports
                     |  uses only the Store interface
              Store interface (TypeScript trait/port)
               /                     \
   Interim store (option B/C)     Burger Oasis engine (Rust daemon, local + remote)
```

The business API and the existing pricing/order/status modules in `web/src/modules/orders` already follow this shape (`OrderRepository`), so they carry forward.

### 4.2 Engine design decisions (for the Rust engine)

| Area | Decision | Reason |
|---|---|---|
| Concurrency | **Single writer, many readers** with serialisable transactions first. MVCC deferred | Far simpler to make correct; a single restaurant's write volume is small |
| Pages | Fixed 8 KB pages, page 0 = header (magic, format version, page size, checksum) | Predictable I/O, simple free-list |
| Integrity | CRC-32C checksum per page and per WAL record | Detect torn writes and bit rot; never silently accept bad data |
| WAL | Append-only log, record = {LSN, txid, type, payload, checksum}; commit record + `fsync` before acknowledging | Standard ARIES-style redo; incomplete transactions ignored on recovery |
| Recovery | On start: scan WAL, redo committed, discard uncommitted, verify checksums, report any corruption (never auto-delete) | Matches spec 4.5/4.6 |
| Indexes | B+ tree for primary key and secondary indexes, updated in the same transaction as the record | Index/record consistency by construction; verifier tool |
| Query | **Structured JSON operations first** (create collection, insert, get, update, delete, filter, sort, paginate, create index, transaction). SQL is explicitly **out of scope** | Spec 4.2 says do not claim SQL |
| Format docs | Versioned file format, documented byte layout | Spec section 16 |
| Daemon | Local HTTP/IPC API bound to localhost; remote node behind TLS reverse proxy | Browsers never read files |

### 4.3 Offline-first sync design

- **Event log is the source of truth for changes.** Every business change is an immutable event: `{eventId (UUIDv7), deviceId, txId, entity, entityId, entityVersion, type, payload, createdAt, hlc}`.
- **Durable outbox** on each device (written in the same transaction as the business change) and a **durable inbox** on the receiver with a unique constraint on `eventId` (idempotent apply).
- **Checkpoints:** the receiver acknowledges the highest contiguous sequence per device, so transfers resume after a crash.
- **Retries** with exponential backoff and jitter; poison events go to a **dead-letter queue** with an operator screen.
- **Device registration:** each device gets an identity keypair; the server can revoke a device immediately.

### 4.4 Domain-specific conflict rules (no universal last-write-wins)

| Domain | Rule |
|---|---|
| Orders | Order id is a globally unique UUID created at the origin, so duplicates are impossible. Status changes follow the state machine; out-of-order events are queued until their predecessor arrives |
| Inventory | Append-only **stock-movement ledger**; stock is the sum of movements. Each movement has a unique id and is applied once. Negative stock is flagged as an oversell conflict for a manager to resolve |
| Payments | Provider reference is unique. Only a **server-side verified** provider result (or an authorised cash confirmation) sets `PAID`. An offline sale is recorded as "cash received, pending upload", never as an electronic payment success |
| Products/prices | Versioned. Concurrent edits to the same version are flagged and need an authorised person to resolve |
| Staff/permissions | Central authority only; devices cannot grant themselves access |

### 4.5 Security

Argon2id password hashing, role-based permissions enforced in the API and again at the store layer, TLS for all remote links, device keys, rate limiting, audit log, encrypted backups, secrets only from environment or a secrets manager, no card data stored (the payment provider holds it).

### 4.6 Backup and recovery targets (proposed, to be confirmed by you)

3-2-1: copy on the laptop, copy on the server, independent third copy offsite. Proposed **RPO 15 minutes** (continuous event shipping), **RTO 1 hour**. A backup is only called good after an automated **restore test** verifies it.

---

## 5. Data model

The domain model already designed in `web/prisma/schema.prisma` (branches, users/roles, products, sizes, extras, promos, orders, order items, payments, order events, ingredients, recipes, stock movements, reservations, audit log) is the starting reference. Additions needed for this spec:

`Device`, `SyncEvent` (outbox/inbox), `SyncCheckpoint`, `Conflict`, `CashierSession`, `CashDrawerEvent`, `KitchenTicket`, `Shift`, `Attendance`, `Supplier`, `PurchaseOrder`, `GoodsReceipt`, `Refund`, `ProviderResponse`, `Backup`.

Money stays integer minor units. Orders from every channel share one table.

---

## 6. Migration plan for the existing site

1. Keep the live static site running unchanged until a real backend exists (it works as a demo storefront).
2. Define the `Store` interface and port the tested pricing/order/status modules onto it.
3. Stand up the Business API against the chosen store (interim or engine).
4. Move the storefront to call the API (cart, checkout, accounts) in small steps, keeping the static site as a fallback.
5. Add Paystack with server-side verification.
6. Build admin, kitchen and POS on the same API.
7. Only then retire `localStorage` order storage.

No file is deleted or replaced during this plan without approval.

---

## 7. Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Engine bugs cause lost or duplicated sales/stock | Critical | Gate real use behind acceptance tests; crash-injection tests; option B/C interim store |
| Scope: engine + sync + business + 5 apps is a very large project | High | Strict phases; each phase shippable and tested |
| Rust toolchain not installed, 7.8 GB RAM laptop | Medium | Install rustup and build tools; keep builds incremental |
| Time-skew and offline devices causing ordering issues | High | Hybrid logical clocks, causal ordering, tests for out-of-order delivery |
| Payment correctness | Critical | Server-side verification only; idempotency keys; reconciliation job |
| Single laptop as the only local node (hardware failure) | High | Continuous event shipping to the server; 3-2-1 backups; tested restore |
| Deploying to production too early | Critical | Rule 13/14 gates; staging node first |

---

## 8. Roadmap and acceptance gates

| Phase | Deliverable | Gate to move on |
|---|---|---|
| 1 | This document, approved decisions | Your sign-off on Section 9 |
| 2 | Rust workspace; page storage; record format; WAL; basic ops; B+ tree | Persistence + reopen tests pass |
| 3 | Recovery, checkpoints, integrity checker, crash-injection tests, concurrency | All crash scenarios recover; corruption detected, never silently lost |
| 4 | Business data model and domain services (products, orders, payments, inventory, POS, kitchen, staff, reports) | Business tests incl. "two cashiers, last item" pass |
| 5 | Local daemon, local API and auth, outbox, offline POS | POS sells with the network cable unplugged |
| 6 | Remote node, device registration, sync, conflict resolution | Duplicate/interrupted sync tests pass |
| 7 | Control Centre console (explorer, query, backup, sync, security, monitoring) wired to real functions | No decorative controls |
| 8 | Website/POS/admin/kitchen integration | One shared data model verified end to end |
| 9 | Security, load, disaster-recovery, payment reconciliation tests | No critical defects open |
| 10 | Production readiness report | All Part 18 criteria demonstrated |

Phases 2-3 alone (a single-node engine I would trust) are the largest unknown. I will report measured results after each phase and will not claim readiness early.

---

## 9. Decisions needed before Phase 2

1. **Storage strategy:** Option A, B (recommended) or C from Section 3.
2. **Rust setup:** approve installing rustup and C++ build tools on this laptop (or choose a different build machine).
3. **Real business data:** name, address, phone, hours, delivery area/fees, real menu and prices, currency (GHS assumed).
4. **Remote server:** which provider for the Linux node (VPS), and a domain for TLS.
5. **Payment provider:** confirm Paystack, and who owns the account.
6. **Recovery targets:** confirm RPO 15 min / RTO 1 hour or set your own.
