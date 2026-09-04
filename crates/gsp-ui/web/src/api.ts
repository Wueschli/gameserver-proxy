// Thin fetch wrappers against gsp-ui's own API. Every call includes the
// session cookie (`credentials: "same-origin"`) and nothing else — this
// frontend never holds or sends a bearer token; gsp-ui presents those to
// the controller/aggregator itself (docs/10 "The admin GUI").

import type { FanoutResponse, RevisionSummary } from "./types";

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request(path: string, init?: RequestInit): Promise<Response> {
  const resp = await fetch(path, { credentials: "same-origin", ...init });
  return resp;
}

async function requestJson<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await request(path, init);
  const text = await resp.text();
  if (!resp.ok) {
    let message = text;
    try {
      const body = JSON.parse(text);
      if (typeof body?.error === "string") message = body.error;
    } catch {
      // not JSON; use the raw text as-is
    }
    throw new ApiError(resp.status, message || resp.statusText);
  }
  return text ? (JSON.parse(text) as T) : (undefined as T);
}

async function requestText(path: string, init?: RequestInit): Promise<string> {
  const resp = await request(path, init);
  const text = await resp.text();
  if (!resp.ok) {
    throw new ApiError(resp.status, text || resp.statusText);
  }
  return text;
}

// --- session ---

// gsp-ui's POST /ui/login and /ui/logout both return a plain-text "ok" body
// on success (see crates/gsp-ui/src/api.rs), not JSON — requestText (not
// requestJson) is required here, or a successful login throws trying to
// JSON.parse("ok").

export async function login(password: string): Promise<void> {
  await requestText("/ui/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ password }),
  });
}

export async function logout(): Promise<void> {
  await requestText("/ui/logout", { method: "POST" });
}

export async function checkSession(): Promise<boolean> {
  const resp = await request("/ui/session");
  return resp.ok;
}

// --- fleet (proxied to gsp-aggregator) ---

export function drainInstance(instance: string): Promise<string> {
  return requestText(`/api/fleet/instances/${encodeURIComponent(instance)}/drain`, {
    method: "POST",
  });
}

export function undrainInstance(instance: string): Promise<string> {
  return requestText(`/api/fleet/instances/${encodeURIComponent(instance)}/undrain`, {
    method: "POST",
  });
}

export function addBackend(pool: string, addr: string): Promise<FanoutResponse> {
  return requestJson(`/api/fleet/pools/${encodeURIComponent(pool)}/backends`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ addr }),
  });
}

export function patchBackend(
  pool: string,
  addr: string,
  state: "enabled" | "draining" | "disabled",
): Promise<FanoutResponse> {
  return requestJson(
    `/api/fleet/pools/${encodeURIComponent(pool)}/backends/${encodeURIComponent(addr)}`,
    {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ state }),
    },
  );
}

export function deleteBackend(pool: string, addr: string): Promise<FanoutResponse> {
  return requestJson(
    `/api/fleet/pools/${encodeURIComponent(pool)}/backends/${encodeURIComponent(addr)}`,
    { method: "DELETE" },
  );
}

export function routeHint(srcIp: string, pool: string, ttlSec: number): Promise<FanoutResponse> {
  return requestJson("/api/fleet/route-hint", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ src_ip: srcIp, pool, ttl_sec: ttlSec }),
  });
}

// --- config (proxied to gsp-controller) ---

export interface CurrentConfig {
  text: string;
  revision: string | null;
}

export async function getCurrentConfig(): Promise<CurrentConfig> {
  const resp = await request("/api/config");
  const text = await resp.text();
  if (!resp.ok) {
    throw new ApiError(resp.status, text || resp.statusText);
  }
  return { text, revision: resp.headers.get("x-config-revision") };
}

export function submitConfig(text: string): Promise<{ revision: number }> {
  return requestJson("/api/config", { method: "POST", body: text });
}

export function listRevisions(): Promise<RevisionSummary[]> {
  return requestJson("/api/config/revisions");
}

export function getRevision(revision: number): Promise<string> {
  return requestText(`/api/config/revisions/${revision}`);
}

export function diffRevision(revision: number, against?: number): Promise<string> {
  const q = against !== undefined ? `?against=${against}` : "";
  return requestText(`/api/config/revisions/${revision}/diff${q}`);
}

export function rollbackTo(revision: number): Promise<{ revision: number }> {
  return requestJson(`/api/config/rollback/${revision}`, { method: "POST" });
}
