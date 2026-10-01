import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, checkSession, getCurrentConfig, login, rollbackTo } from "./api";

function mockFetch(resp: Response) {
  const fn = vi.fn().mockResolvedValue(resp);
  vi.stubGlobal("fetch", fn);
  return fn;
}

afterEach(() => vi.unstubAllGlobals());

describe("api", () => {
  it("login accepts gsp-ui's plain-text `ok` body (not JSON)", async () => {
    mockFetch(new Response("ok", { status: 200 }));
    await expect(login("pw")).resolves.toBeUndefined();
  });

  it("surfaces a JSON `error` field as the ApiError message", async () => {
    mockFetch(new Response(JSON.stringify({ error: "no such revision" }), { status: 404 }));
    const err = await rollbackTo(9).catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(404);
    expect(err.message).toBe("no such revision");
  });

  it("falls back to the raw text when the error body is not JSON", async () => {
    mockFetch(new Response("upstream down", { status: 502 }));
    const err = await rollbackTo(9).catch((e) => e);
    expect(err.message).toBe("upstream down");
  });

  it("getCurrentConfig reads the X-Config-Revision header the proxy must forward", async () => {
    mockFetch(new Response("pools: []", { status: 200, headers: { "X-Config-Revision": "7" } }));
    await expect(getCurrentConfig()).resolves.toEqual({ text: "pools: []", revision: "7" });
  });

  it("always sends the session cookie and nothing else", async () => {
    const fn = mockFetch(new Response("", { status: 200 }));
    await checkSession();
    expect(fn).toHaveBeenCalledWith("/ui/session", { credentials: "same-origin" });
  });
});
