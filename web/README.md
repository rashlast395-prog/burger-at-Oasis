# Burger Oasis platform (Stage 2 foundation)

Next.js (App Router) + TypeScript (strict) + PostgreSQL via Prisma. This folder is the **new** platform.
The live storefront (`../Burger/sarab`, static HTML) is unchanged and still what Vercel serves.

## What exists

| Area | Status |
|---|---|
| Database schema (`prisma/schema.prisma`) | Done, validated. Orders, products, sizes/extras, promos, payments, stock, recipes, reservations, audit log, branches, roles |
| Server-side pricing (`src/modules/orders/pricing.ts`) | Done, tested. Browser never sets prices |
| Order creation with duplicate protection (`order.service.ts`) | Done, tested against an in-memory repository |
| Order status state machine (`status.ts`) | Done, tested |
| API: `GET /api/menu`, `POST /api/orders/quote`, `POST /api/orders` | Run end to end against the **engine store** on a laptop. Not yet run against PostgreSQL |
| Seed scripts (`prisma/seed.ts`, `scripts/engine-seed.ts`) | Engine seed run; Prisma seed not yet run. Shared placeholder menu in `prisma/menu-data.ts` |
| Order store selection (`src/server/repositories/index.ts`) | `ORDER_STORE=prisma` (default) or `engine` |
| Storefront (`/`): menu, sizes/extras, cart, server-priced totals, promo codes, checkout | Done, tested in the browser. Cash on delivery/collection only |
| Staff order board (`/staff`): sign-in, live orders, status changes, record cash | Done, tested in the browser. **Engine store only** for now |
| Auth, payments (Paystack), admin, POS, kitchen, inventory deduction, notifications | Not started |

## Key decisions

- **Money is integer minor units** (`1499` = 14.99). Currency is `NEXT_PUBLIC_CURRENCY`.
- **One order table for every channel** (`channel`: WEB, POS, PHONE) so sales, stock and reports share one source of truth.
- **Prices are snapshotted** onto `OrderItem` so later menu edits never change past orders.
- **Idempotency**: `POST /api/orders` requires `idempotencyKey`; replaying it returns the original order (HTTP 200) instead of creating a second one.
- **Business logic is separate from the database** (`OrderRepository` interface) so it can be tested without Postgres.
- **Single branch today, multi-branch ready**: `branchId` is on orders and ingredients.

## Setup

```bash
cd web
cp .env.example .env.local     # fill DATABASE_URL
npm install
npx prisma migrate dev --name init
npm run db:seed
npm run dev
```

## Run on this laptop with the engine (no PostgreSQL needed)

The custom Burger Oasis engine (`../engine`) can hold orders instead of PostgreSQL. It is
**experimental**: use it for testing on a laptop, not for real orders. The site refuses it in
production unless `ALLOW_EXPERIMENTAL_ENGINE=yes` is set deliberately.

1. Make a random token and put it in `web/.env.local` (git-ignored):

   ```
   ORDER_STORE=engine
   BO_ENGINE_URL=http://127.0.0.1:7480
   BO_API_TOKEN=<64 random hex characters>
   ```

   PowerShell to generate one:
   `-join ((1..32) | ForEach-Object { '{0:x2}' -f (Get-Random -Maximum 256) })`

2. Start the engine once set up (`bo-server init <dir> --host localhost`), with the same token:

   ```powershell
   $env:BO_API_TOKEN = "<the same token>"
   bo-server run <dir> --api 127.0.0.1:7480
   ```

3. Load the menu, then start the site:

   ```bash
   npm run engine:seed   # reads BO_API_TOKEN / BO_ENGINE_URL from the environment
   npm run dev
   ```

4. Staff board at `/staff`. Add to `web/.env.local`:

   ```
   STAFF_PASSWORD=<12+ characters; share only with staff>
   STAFF_SESSION_SECRET=<64 random hex characters>
   ```

   One shared password for now (per-person accounts come with the auth stage). Sessions are
   signed, HttpOnly, SameSite=Strict cookies lasting 12 hours; five wrong passwords lock
   sign-in for 15 minutes.

The engine API only listens on 127.0.0.1 and needs the token. It enforces idempotency and promo
limits atomically and re-checks every total the site calculates. Order statuses map as
placed→PENDING, accepted→CONFIRMED, preparing, ready, out_for_delivery, completed, cancelled;
a partly paid order is reported as UNPAID.

## Checks

```bash
npm test          # unit tests
npm run typecheck
npm run lint
npm run build
```

## Next stages

1. Customer auth and sessions, protected admin routes (role checks on the server).
2. Paystack: initiate payment, verify via webhook and server-side verify, never trust the redirect.
3. Port the storefront UI to React using these APIs.
4. Admin dashboard, kitchen display, stock deduction from recipes, POS.
