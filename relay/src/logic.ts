// Pure helpers: no Workers APIs, so bun test can run them directly.

export const MAX_BODY = 64 * 1024;
// Graph's validation token is a short opaque string; anything longer is not Graph.
export const MAX_VALIDATION_TOKEN = 1024;
// Shorter secrets are guessable, and an unset secret must never look configured.
export const MIN_SECRET = 16;
const MAX_FIELD = 512;

export type Route =
  | { kind: "health" | "notify" | "lifecycle" | "ws" }
  | { kind: "notfound" }
  | { kind: "method"; allow: string };

const ROUTES: Record<string, { kind: "health" | "notify" | "lifecycle" | "ws"; method: string }> = {
  "/health": { kind: "health", method: "GET" },
  "/graph/notify": { kind: "notify", method: "POST" },
  "/graph/lifecycle": { kind: "lifecycle", method: "POST" },
  "/ws": { kind: "ws", method: "GET" },
};

export function route(method: string, pathname: string): Route {
  const r = Object.hasOwn(ROUTES, pathname) ? ROUTES[pathname] : undefined;
  if (!r) return { kind: "notfound" };
  if (method !== r.method) return { kind: "method", allow: r.method };
  return { kind: r.kind };
}

// Graph's endpoint check: echo the token back as text/plain.
export function validationToken(url: URL): string | null {
  return url.searchParams.get("validationToken");
}

// True only for a secret that was actually set to something long enough. Without this,
// an unset secret compares equal to a missing clientState and the Worker accepts forgeries.
export function isConfigured(secret: unknown): secret is string {
  return typeof secret === "string" && secret.length >= MIN_SECRET;
}

// Compares every byte of the longer input, so timing does not reveal where
// the first difference is or how long the secret is.
export function safeEqual(a: string, b: string): boolean {
  const enc = new TextEncoder();
  const x = enc.encode(a);
  const y = enc.encode(b);
  const n = Math.max(x.length, y.length);
  let diff = x.length ^ y.length;
  for (let i = 0; i < n; i++) diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return diff === 0;
}

export function parseBearer(header: string | null): string | null {
  if (!header) return null;
  const m = /^Bearer +(\S+)$/i.exec(header.trim());
  return m ? m[1] : null;
}

export type NotifyItem = { subscriptionId: string; changeType: string; resource: string };
export type LifecycleItem = { subscriptionId: string; lifecycleEvent: string };
export type Frame =
  | { type: "notify"; items: NotifyItem[] }
  | { type: "lifecycle"; items: LifecycleItem[] }
  | { type: "alert"; text: string };

function str(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 && v.length <= MAX_FIELD ? v : null;
}

// Builds frames from a Graph body. Only ids are copied: clientState and any
// other field is stripped. Items with a wrong clientState are dropped and
// reported in one alert frame.
export function buildFrames(
  kind: "notify" | "lifecycle",
  body: unknown,
  clientState: string,
): Frame[] {
  if (!isConfigured(clientState)) return [];
  const value = (body as { value?: unknown } | null)?.value;
  if (!Array.isArray(value)) return [];
  const notify: NotifyItem[] = [];
  const life: LifecycleItem[] = [];
  let bad = 0;
  for (const raw of value) {
    const it = (raw ?? {}) as Record<string, unknown>;
    const cs = typeof it.clientState === "string" ? it.clientState : "";
    if (!safeEqual(cs, clientState)) {
      bad++;
      continue;
    }
    const subscriptionId = str(it.subscriptionId);
    if (!subscriptionId) continue;
    if (kind === "notify") {
      const changeType = str(it.changeType);
      const resource = str(it.resource);
      if (changeType && resource) notify.push({ subscriptionId, changeType, resource });
    } else {
      const lifecycleEvent = str(it.lifecycleEvent);
      if (lifecycleEvent) life.push({ subscriptionId, lifecycleEvent });
    }
  }
  const frames: Frame[] = [];
  if (notify.length) frames.push({ type: "notify", items: notify });
  if (life.length) frames.push({ type: "lifecycle", items: life });
  if (bad) {
    frames.push({
      type: "alert",
      text: `${bad} notification(s) with a wrong clientState were dropped. Check CLIENT_STATE on the Worker matches chatcli.`,
    });
  }
  return frames;
}

// Reads a body, refusing anything over MAX_BODY even when Content-Length lies
// or is absent. Returns null when too large.
export async function readLimited(req: Request, max = MAX_BODY): Promise<string | null> {
  const declared = Number(req.headers.get("content-length") ?? "0");
  if (declared > max) return null;
  if (!req.body) return "";
  const reader = req.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.length;
    if (total > max) {
      await reader.cancel();
      return null;
    }
    chunks.push(value);
  }
  const all = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    all.set(c, off);
    off += c.length;
  }
  return new TextDecoder().decode(all);
}
