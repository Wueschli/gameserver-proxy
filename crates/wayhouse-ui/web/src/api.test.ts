import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  checkSession,
  getCurrentConfig,
  installFromRegistry,
  login,
  releaseTunnelAddress,
  rollbackTo,
} from "./api";

function mockFetch(resp: Response) {
  const fn = vi.fn().mockResolvedValue(resp);
  vi.stubGlobal("fetch", fn);
  return fn;
}

afterEach(() => vi.unstubAllGlobals());

describe("api", () => {
  it("login accepts wayhouse-ui's plain-text `ok` body (not JSON)", async () => {
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

  it("releaseTunnelAddress DELETEs the role's route with the name encoded as one segment", async () => {
    const fn = vi
      .fn()
      .mockImplementation(async () => new Response('{"revision":3,"released":"10.200.0.2"}', { status: 200 }));
    vi.stubGlobal("fetch", fn);
    await expect(releaseTunnelAddress("origin", "a/b c")).resolves.toEqual({
      revision: 3,
      released: "10.200.0.2",
    });
    expect(fn.mock.calls[0][0]).toBe("/api/tunnel/origins/a%2Fb%20c");
    expect(fn.mock.calls[0][1]).toMatchObject({ method: "DELETE" });
    await releaseTunnelAddress("proxy", "edge-1");
    expect(fn.mock.calls[1][0]).toBe("/api/tunnel/proxies/edge-1");
  });

  it("always sends the session cookie and nothing else", async () => {
    const fn = mockFetch(new Response("", { status: 200 }));
    await checkSession();
    expect(fn).toHaveBeenCalledWith("/ui/session", { credentials: "same-origin" });
  });

  it("installFromRegistry returns the per-instance body of a 502 (nothing accepted) instead of throwing", async () => {
    const body = { sniffer: "demo", results: [{ instance: "a", ok: false, pinned: false, error: "refused", status: null }], pinned_instances: [] };
    const fn = mockFetch(new Response(JSON.stringify(body), { status: 502 }));
    await expect(installFromRegistry("abc", "demo")).resolves.toMatchObject({ results: [{ instance: "a" }] });
    expect(fn.mock.calls[0][0]).toBe("/api/registries/abc/install");
    expect(JSON.parse(fn.mock.calls[0][1].body)).toEqual({ name: "demo" });
  });

  it("installFromRegistry still throws a plain error body (registry unreachable)", async () => {
    mockFetch(new Response(JSON.stringify({ error: "registry: timed out" }), { status: 502 }));
    const err = await installFromRegistry("abc", "demo").catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.message).toBe("registry: timed out");
  });
});
