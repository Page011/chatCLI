import { describe, expect, test } from "bun:test";
import {
  buildFrames,
  isConfigured,
  parseBearer,
  readLimited,
  route,
  safeEqual,
  validationToken,
} from "../src/logic";

describe("route", () => {
  test("known routes", () => {
    expect(route("GET", "/health")).toEqual({ kind: "health" });
    expect(route("POST", "/graph/notify")).toEqual({ kind: "notify" });
    expect(route("POST", "/graph/lifecycle")).toEqual({ kind: "lifecycle" });
    expect(route("GET", "/ws")).toEqual({ kind: "ws" });
  });
  test("404 and 405", () => {
    expect(route("GET", "/nope")).toEqual({ kind: "notfound" });
    expect(route("GET", "/constructor")).toEqual({ kind: "notfound" });
    expect(route("GET", "/graph/notify")).toEqual({ kind: "method", allow: "POST" });
    expect(route("POST", "/ws")).toEqual({ kind: "method", allow: "GET" });
  });
});

test("validationToken", () => {
  expect(validationToken(new URL("https://x/graph/notify?validationToken=a%20b"))).toBe("a b");
  expect(validationToken(new URL("https://x/graph/notify"))).toBeNull();
});

test("safeEqual", () => {
  expect(safeEqual("abc", "abc")).toBe(true);
  expect(safeEqual("abc", "abd")).toBe(false);
  expect(safeEqual("abc", "abcd")).toBe(false);
  expect(safeEqual("", "")).toBe(true);
  expect(safeEqual("", "a")).toBe(false);
});

test("parseBearer", () => {
  expect(parseBearer("Bearer tok")).toBe("tok");
  expect(parseBearer("bearer tok")).toBe("tok");
  expect(parseBearer("Basic tok")).toBeNull();
  expect(parseBearer("Bearer")).toBeNull();
  expect(parseBearer(null)).toBeNull();
});

const CS = "client-state-0123456789";
describe("buildFrames", () => {
  const good = { subscriptionId: "s1", changeType: "created", resource: "chats('19:a')/messages('1')", clientState: CS };
  test("notify strips clientState and extra fields", () => {
    const f = buildFrames("notify", { value: [{ ...good, resourceData: { secret: 1 } }] }, CS);
    expect(f).toEqual([
      { type: "notify", items: [{ subscriptionId: "s1", changeType: "created", resource: "chats('19:a')/messages('1')" }] },
    ]);
    expect(JSON.stringify(f)).not.toContain("clientState");
  });
  test("lifecycle", () => {
    const f = buildFrames("lifecycle", { value: [{ subscriptionId: "s1", lifecycleEvent: "missed", clientState: CS }] }, CS);
    expect(f).toEqual([{ type: "lifecycle", items: [{ subscriptionId: "s1", lifecycleEvent: "missed" }] }]);
  });
  test("bad clientState dropped with alert, good kept", () => {
    const f = buildFrames("notify", { value: [good, { ...good, clientState: "bad" }, { ...good, clientState: undefined }] }, CS);
    expect(f.length).toBe(2);
    expect(f[1]).toMatchObject({ type: "alert" });
    expect((f[1] as { text: string }).text).toContain("2 notification");
    expect(JSON.stringify(f)).not.toContain("bad\"");
  });
  test("unset or short secret fails closed", () => {
    const body = { value: [{ subscriptionId: "s1", changeType: "created", resource: "r" }] };
    expect(buildFrames("notify", body, undefined as unknown as string)).toEqual([]);
    expect(buildFrames("notify", body, "")).toEqual([]);
    expect(buildFrames("notify", { value: [{ ...good, clientState: "cs" }] }, "cs")).toEqual([]);
    expect(isConfigured(undefined)).toBe(false);
    expect(isConfigured(CS)).toBe(true);
  });
  test("malformed bodies give no frames", () => {
    expect(buildFrames("notify", null, CS)).toEqual([]);
    expect(buildFrames("notify", { value: "x" }, CS)).toEqual([]);
    expect(buildFrames("notify", { value: [{ clientState: CS }] }, CS)).toEqual([]);
  });
});

describe("readLimited", () => {
  const post = (body: string, headers: Record<string, string> = {}) =>
    new Request("https://x/", { method: "POST", body, headers });
  test("reads small body", async () => {
    expect(await readLimited(post("hello"), 10)).toBe("hello");
  });
  test("refuses over limit", async () => {
    expect(await readLimited(post("x".repeat(11)), 10)).toBeNull();
  });
  test("refuses on declared length", async () => {
    expect(await readLimited(post("a", { "content-length": "999" }), 10)).toBeNull();
  });
});
