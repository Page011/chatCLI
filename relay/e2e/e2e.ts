// End-to-end test of the relay: starts `wrangler dev` in local mode with random secrets, then
// checks every rule of "Relay protocol" in docs/DESIGN.md over real HTTP and a real WebSocket.
//
//   cd relay && bun run test:e2e
//
// Local only: nothing here talks to Cloudflare or Microsoft. The file is named e2e.ts (not
// *.test.ts) so `bun test` does not pick it up in the fast unit job. Secrets are random per run
// and passed on the command line of the local dev server only, never written to a file.

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const HEALTH_DEADLINE_MS = 120_000; // workerd is slow to start on a cold CI runner
const FRAME_WAIT_MS = 5_000;
const QUIET_MS = 600; // how long "nothing else arrives" is watched

const TOKEN = `t-${crypto.randomUUID()}`;
const CLIENT_STATE = `c-${crypto.randomUUID()}`;

type Frame = Record<string, unknown>;

async function freePort(): Promise<number> {
  const s = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: () => new Response("") });
  const port = s.port as number;
  await s.stop(true);
  return port;
}

let dev: ChildProcess | null = null;
let output = "";

function startWrangler(port: number, persist: string): void {
  // `bun x` runs the wrangler pinned in relay/package.json. Own process group on POSIX so the
  // whole tree (wrangler, workerd) can be killed together.
  dev = spawn(
    process.execPath,
    [
      "x", "wrangler", "dev", "--local", "--ip", "127.0.0.1", "--port", String(port),
      "--persist-to", persist,
      "--var", `RELAY_TOKEN:${TOKEN}`, "--var", `CLIENT_STATE:${CLIENT_STATE}`,
    ],
    {
      cwd: join(import.meta.dir, ".."),
      env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1" },
      detached: process.platform !== "win32",
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  const keep = (b: Buffer) => {
    output = (output + b.toString()).slice(-4000);
  };
  dev.stdout?.on("data", keep);
  dev.stderr?.on("data", keep);
}

function stopWrangler(): void {
  const pid = dev?.pid;
  if (!pid) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    try {
      process.kill(-pid, "SIGKILL");
    } catch {
      // already gone
    }
  }
  dev = null;
}

async function waitForHealth(base: string): Promise<void> {
  const end = Date.now() + HEALTH_DEADLINE_MS;
  while (Date.now() < end) {
    if (dev?.exitCode !== null && dev?.exitCode !== undefined) {
      throw new Error(`wrangler dev exited with code ${dev.exitCode}.\n${output}`);
    }
    try {
      const r = await fetch(`${base}/health`);
      if (r.ok) return;
    } catch {
      // not up yet
    }
    await Bun.sleep(500);
  }
  throw new Error(`/health did not answer within ${HEALTH_DEADLINE_MS / 1000} s.\n${output}`);
}

// A WebSocket with a frame queue, so a test can ask "the next frame" or "nothing for a while".
class Sock {
  frames: string[] = [];
  closed: { code: number } | null = null;
  private waiters: (() => void)[] = [];
  private ws: WebSocket;
  opened: Promise<void>;

  constructor(url: string, token: string) {
    // Bun accepts request headers as an option; the browser type does not know it.
    this.ws = new (WebSocket as unknown as new (u: string, o: unknown) => WebSocket)(url, {
      headers: { authorization: `Bearer ${token}` },
    });
    this.opened = new Promise((res, rej) => {
      this.ws.onopen = () => res();
      this.ws.onerror = () => rej(new Error("socket error"));
    });
    this.ws.onmessage = (e) => {
      this.frames.push(String(e.data));
      this.wake();
    };
    this.ws.onclose = (e) => {
      this.closed = { code: e.code };
      this.wake();
    };
  }

  private wake() {
    for (const w of this.waiters.splice(0)) w();
  }

  private async until(pred: () => boolean, ms: number): Promise<boolean> {
    const end = Date.now() + ms;
    while (!pred()) {
      const left = end - Date.now();
      if (left <= 0) return false;
      await new Promise<void>((res) => {
        this.waiters.push(res);
        setTimeout(res, left);
      });
    }
    return true;
  }

  async next(ms = FRAME_WAIT_MS): Promise<Frame> {
    if (!(await this.until(() => this.frames.length > 0, ms))) throw new Error("no frame arrived");
    return JSON.parse(this.frames.shift() as string) as Frame;
  }

  async nextText(ms = FRAME_WAIT_MS): Promise<string> {
    if (!(await this.until(() => this.frames.length > 0, ms))) throw new Error("no frame arrived");
    return this.frames.shift() as string;
  }

  async quiet(ms = QUIET_MS): Promise<boolean> {
    await this.until(() => this.frames.length > 0, ms);
    return this.frames.length === 0;
  }

  async waitClosed(ms = FRAME_WAIT_MS): Promise<number> {
    if (!(await this.until(() => this.closed !== null, ms))) throw new Error("socket stayed open");
    return (this.closed as { code: number }).code;
  }

  send(text: string) {
    this.ws.send(text);
  }

  close() {
    try {
      this.ws.close();
    } catch {
      // already closed
    }
  }
}

const results: { name: string; error?: string }[] = [];
async function step(name: string, fn: () => Promise<void>): Promise<void> {
  try {
    await fn();
    results.push({ name });
    console.log(`ok    ${name}`);
  } catch (e) {
    results.push({ name, error: String(e) });
    console.log(`FAIL  ${name}: ${e}`);
  }
}

function eq(got: unknown, want: unknown, what: string): void {
  if (JSON.stringify(got) !== JSON.stringify(want)) {
    throw new Error(`${what}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
  }
}

function graphBody(items: Record<string, unknown>[]): string {
  return JSON.stringify({ value: items });
}

const notifyItem = (clientState: string, id = "sub-1") => ({
  subscriptionId: id,
  changeType: "created",
  resource: "chats('19:example@thread.v2')/messages('1700000000001')",
  clientState,
  // Real Graph can carry more than ids; the relay must not forward any of it.
  resourceData: { body: "SHOULD-NEVER-BE-FORWARDED" },
});

async function main(): Promise<void> {
  const persist = mkdtempSync(join(tmpdir(), "chatcli-relay-e2e-"));
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  const wsUrl = `ws://127.0.0.1:${port}/ws`;
  const post = (path: string, body: string) => fetch(`${base}${path}`, { method: "POST", body });

  try {
    startWrangler(port, persist);
    await waitForHealth(base);

    await step("GET /health returns ok", async () => {
      const r = await fetch(`${base}/health`);
      eq([r.status, await r.text()], [200, "ok"], "health");
    });

    await step("validationToken is echoed as text/plain", async () => {
      const r = await post("/graph/notify?validationToken=abc", "");
      eq([r.status, await r.text()], [200, "abc"], "echo");
      eq(r.headers.get("content-type")?.startsWith("text/plain"), true, "content type");
    });

    // Order matters: this must run before any socket connects, so the relay has nobody to tell.
    await step("notifications with no socket are counted in hello.dropped", async () => {
      for (let i = 0; i < 2; i++) {
        const r = await post("/graph/notify", graphBody([notifyItem(CLIENT_STATE)]));
        eq(r.status, 202, "answer to Graph");
      }
    });

    let first: Sock | null = null;

    await step("GET /ws: wrong or missing token is 401, no upgrade is 426", async () => {
      const bad = await fetch(`${base}/ws`, { headers: { authorization: "Bearer wrong" } });
      eq(bad.status, 401, "wrong token");
      eq((await fetch(`${base}/ws`)).status, 401, "no token");
      const plain = await fetch(`${base}/ws`, { headers: { authorization: `Bearer ${TOKEN}` } });
      eq(plain.status, 426, "right token, no upgrade");
    });

    await step("GET /ws with the right token gets hello with the dropped count", async () => {
      first = new Sock(wsUrl, TOKEN);
      await first.opened;
      eq(await first.next(), { type: "hello", dropped: 2 }, "hello");
    });

    await step("ping gets pong", async () => {
      (first as Sock).send("ping");
      eq(await (first as Sock).nextText(), "pong", "reply");
    });

    await step("a good notification arrives as a notify frame with ids only", async () => {
      const r = await post("/graph/notify", graphBody([notifyItem(CLIENT_STATE, "sub-9")]));
      eq(r.status, 202, "answer to Graph");
      const raw = await (first as Sock).nextText();
      eq(raw.includes("SHOULD-NEVER-BE-FORWARDED"), false, "message body leaked");
      eq(raw.includes(CLIENT_STATE), false, "client state leaked");
      eq(JSON.parse(raw), {
        type: "notify",
        items: [
          {
            subscriptionId: "sub-9",
            changeType: "created",
            resource: "chats('19:example@thread.v2')/messages('1700000000001')",
          },
        ],
      }, "frame");
    });

    await step("a lifecycle notification arrives as a lifecycle frame", async () => {
      const r = await post(
        "/graph/lifecycle",
        graphBody([{ subscriptionId: "sub-9", lifecycleEvent: "reauthorizationRequired", clientState: CLIENT_STATE }]),
      );
      eq(r.status, 202, "answer to Graph");
      eq(await (first as Sock).next(), {
        type: "lifecycle",
        items: [{ subscriptionId: "sub-9", lifecycleEvent: "reauthorizationRequired" }],
      }, "frame");
    });

    await step("a wrong clientState gives one alert frame and nothing else", async () => {
      const r = await post("/graph/notify", graphBody([notifyItem("not-the-secret")]));
      eq(r.status, 202, "answer to Graph");
      const f = await (first as Sock).next();
      eq(f.type, "alert", "frame type");
      eq(String(f.text).includes("not-the-secret"), false, "alert repeats the bad value");
      eq(await (first as Sock).quiet(), true, "extra frames after the alert");
    });

    await step("a second socket closes the first with code 4000", async () => {
      const second = new Sock(wsUrl, TOKEN);
      await second.opened;
      eq(await second.next(), { type: "hello", dropped: 0 }, "hello on the new socket");
      eq(await (first as Sock).waitClosed(), 4000, "close code of the old socket");
      second.close();
    });
  } finally {
    stopWrangler();
    // The tree must be gone: nothing may still answer on the port.
    await Bun.sleep(1000);
    const alive = await fetch(`${base}/health`).then(() => true, () => false);
    if (alive) {
      results.push({ name: "wrangler stopped", error: "something still answers on the dev port" });
      console.log("FAIL  wrangler stopped: something still answers on the dev port");
    }
    try {
      rmSync(persist, { recursive: true, force: true });
    } catch {
      // a locked file on Windows is only temp data
    }
  }

  const failed = results.filter((r) => r.error);
  console.log(`${results.length - failed.length} passed, ${failed.length} failed.`);
  process.exit(failed.length ? 1 : 0);
}

main().catch((e) => {
  stopWrangler();
  console.error(String(e));
  process.exit(1);
});
