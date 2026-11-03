/**
 * Minimal client for the Burger Oasis engine's local website API (`bo-server run --api`).
 * Server-side only: it needs BO_API_TOKEN, which must never reach the browser.
 */

export class EngineError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "EngineError";
  }
}

const TIMEOUT_MS = 5000;

function baseUrl(): string {
  return (process.env.BO_ENGINE_URL ?? "http://127.0.0.1:7480").replace(/\/$/, "");
}

function token(): string {
  const t = process.env.BO_API_TOKEN;
  if (!t) throw new Error("BO_API_TOKEN is not set: it must match the token bo-server was started with.");
  return t;
}

/**
 * Call the engine. Returns `null` for a 404 when `allowNotFound` is set; any other
 * non-2xx response throws an EngineError carrying the engine's error code.
 */
export async function engine<T>(
  method: "GET" | "PUT" | "POST",
  path: string,
  body?: unknown,
  opts: { allowNotFound?: boolean } = {},
): Promise<T | null> {
  let res: Response;
  try {
    res = await fetch(`${baseUrl()}/v1${path}`, {
      method,
      headers: { Authorization: `Bearer ${token()}`, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(TIMEOUT_MS),
      cache: "no-store",
    });
  } catch (e) {
    throw new EngineError(503, "ENGINE_UNREACHABLE", `The order engine is not reachable at ${baseUrl()}: ${(e as Error).message}`);
  }
  if (res.status === 404 && opts.allowNotFound) return null;
  const json = (await res.json().catch(() => ({}))) as { error?: string; message?: string };
  if (!res.ok) throw new EngineError(res.status, json.error ?? "ENGINE_ERROR", json.message ?? `engine returned ${res.status}`);
  return json as T;
}

/** Drop null/undefined fields: the engine treats a present field as a value to validate. */
export function compact<T extends Record<string, unknown>>(obj: T): Partial<T> {
  return Object.fromEntries(Object.entries(obj).filter(([, v]) => v !== null && v !== undefined)) as Partial<T>;
}
