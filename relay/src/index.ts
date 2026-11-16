import { DurableObject } from "cloudflare:workers";
import {
  buildFrames,
  isConfigured,
  MAX_VALIDATION_TOKEN,
  parseBearer,
  readLimited,
  route,
  safeEqual,
  validationToken,
  type Frame,
} from "./logic";

export interface Env {
  HUB: DurableObjectNamespace<RelayHub>;
  RELAY_TOKEN: string;
  CLIENT_STATE: string;
}

const ALERT_INTERVAL_MS = 60_000;

const text = (body: string, status: number, extra: Record<string, string> = {}) =>
  new Response(body, { status, headers: { "content-type": "text/plain", "x-content-type-options": "nosniff", ...extra } });

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const r = route(request.method, url.pathname);
    if (r.kind === "notfound") return text("not found", 404);
    if (r.kind === "method") return text("method not allowed", 405, { allow: r.allow });
    if (r.kind === "health") return text("ok", 200);

    const hub = env.HUB.get(env.HUB.idFromName("hub"));

    if (r.kind === "ws") {
      if (!isConfigured(env.RELAY_TOKEN)) return text("relay not configured", 503);
      const token = parseBearer(request.headers.get("authorization"));
      if (!token || !safeEqual(token, env.RELAY_TOKEN)) return text("unauthorized", 401);
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return text("websocket upgrade required", 426);
      }
      return hub.fetch("https://hub/connect", request);
    }

    // Graph endpoint check.
    const echo = validationToken(url);
    if (echo !== null) {
      return echo.length > MAX_VALIDATION_TOKEN ? text("bad request", 400) : text(echo, 200);
    }
    if (!isConfigured(env.CLIENT_STATE)) return text("relay not configured", 503);

    const raw = await readLimited(request);
    if (raw === null) return text("payload too large", 413);
    let body: unknown;
    try {
      body = JSON.parse(raw);
    } catch {
      return text("bad request", 400);
    }
    const frames = buildFrames(r.kind, body, env.CLIENT_STATE);
    if (frames.length) {
      await hub.fetch("https://hub/push", { method: "POST", body: JSON.stringify(frames) });
    }
    return text("", 202);
  },
};

export class RelayHub extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // Answered without waking the object, so idle sockets stay cheap.
    this.ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (path === "/connect") return this.accept();
    if (path === "/push") return this.push(await request.json<Frame[]>());
    return new Response("not found", { status: 404 });
  }

  private async accept(): Promise<Response> {
    for (const old of this.ctx.getWebSockets()) old.close(4000, "replaced");
    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    this.ctx.acceptWebSocket(server);
    const dropped = (await this.ctx.storage.get<number>("dropped")) ?? 0;
    server.send(JSON.stringify({ type: "hello", dropped }));
    if (dropped) await this.ctx.storage.put("dropped", 0);
    return new Response(null, { status: 101, webSocket: client });
  }

  private async push(frames: Frame[]): Promise<Response> {
    const sockets = this.ctx.getWebSockets();
    let dropped = 0;
    for (const f of await this.throttleAlerts(frames)) {
      const msg = JSON.stringify(f);
      let delivered = false;
      for (const ws of sockets) {
        // One dead socket must not stop the rest or turn the answer to Graph into a 500.
        try {
          ws.send(msg);
          delivered = true;
        } catch {
          try {
            ws.close(1011, "send failed");
          } catch {
            // already closed
          }
        }
      }
      // Alerts are not notifications to catch up on, so only count the rest.
      if (!delivered && f.type !== "alert") dropped++;
    }
    if (dropped) {
      const cur = (await this.ctx.storage.get<number>("dropped")) ?? 0;
      await this.ctx.storage.put("dropped", cur + dropped);
    }
    return new Response(null, { status: 204 });
  }

  // At most one alert frame a minute, so anyone posting junk cannot flood chatcli's notice
  // line and log. The count of skipped ones rides on the next alert that is let through.
  private async throttleAlerts(frames: Frame[]): Promise<Frame[]> {
    if (!frames.some((f) => f.type === "alert")) return frames;
    const now = Date.now();
    const last = (await this.ctx.storage.get<number>("alertAt")) ?? 0;
    let skipped = (await this.ctx.storage.get<number>("alertSkipped")) ?? 0;
    const out: Frame[] = [];
    let sentAlert = false;
    for (const f of frames) {
      if (f.type !== "alert") {
        out.push(f);
      } else if (!sentAlert && now - last >= ALERT_INTERVAL_MS) {
        sentAlert = true;
        out.push(skipped ? { ...f, text: `${f.text} (${skipped} earlier alert(s) not shown)` } : f);
        skipped = 0;
      } else {
        skipped++;
      }
    }
    if (sentAlert) await this.ctx.storage.put("alertAt", now);
    await this.ctx.storage.put("alertSkipped", skipped);
    return out;
  }

  // chatcli sends nothing but "ping", which the auto-response handles.
  webSocketMessage(): void {}

  webSocketClose(ws: WebSocket, code: number): void {
    // 1005/1006 are reserved and cannot be sent in a close frame.
    try {
      ws.close(code === 1005 || code === 1006 ? 1000 : code);
    } catch {
      // already closed
    }
  }

  webSocketError(ws: WebSocket): void {
    try {
      ws.close(1011, "error");
    } catch {
      // already closed
    }
  }
}
