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
| API: `GET /api/menu`, `POST /api/orders/quote`, `POST /api/orders` | Written, type-checked. **Not yet run against a real database** |
| Seed script (`prisma/seed.ts`) | Written, not yet run. Uses the placeholder menu |
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
