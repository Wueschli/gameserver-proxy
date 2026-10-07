// Thin fetch wrappers against wayhouse-ui's own API. Every call includes the
// session cookie (`credentials: "same-origin"`) and nothing else — this
// frontend never holds or sends a bearer token; wayhouse-ui presents those to
// the controller/aggregator itself (docs/10 "The admin GUI").

import type {
  FanoutResponse,
  InstallResponse,
  RegistryList,
  RegistryRef,
  RegistrySniffers,
  RevisionSummary,
  SnifferInfo,
  TunnelAddresses,
} from "./types";

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

// wayhouse-ui's POST /ui/login and /ui/logout both return a plain-text "ok" body
// on success (see crates/wayhouse-ui/src/api.rs), not JSON — requestText (not
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

// --- fleet (proxied to wayhouse-aggregator) ---

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

// --- sniffers (proxied to wayhouse-aggregator, fanned out to instances) ---

export function listInstanceSniffers(instance: string): Promise<SnifferInfo[]> {
  return requestJson(`/api/fleet/instances/${encodeURIComponent(instance)}/sniffers`);
}

export function uploadSniffer(name: string, bytes: ArrayBuffer): Promise<FanoutResponse> {
  return requestJson(`/api/fleet/sniffers?name=${encodeURIComponent(name)}`, {
    method: "POST",
    headers: { "content-type": "application/octet-stream" },
    body: bytes,
  });
}

// --- sniffer registries (wayhouse-ui's own routes) ---

export function listRegistries(): Promise<RegistryList> {
  return requestJson("/api/registries");
}

export function addRegistry(url: string, name?: string): Promise<RegistryRef> {
  return requestJson("/api/registries", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(name ? { url, name } : { url }),
  });
}

export function removeRegistry(id: string): Promise<void> {
  return requestJson(`/api/registries/${encodeURIComponent(id)}`, { method: "DELETE" });
}

export function listRegistrySniffers(id: string): Promise<RegistrySniffers> {
  return requestJson(`/api/registries/${encodeURIComponent(id)}/sniffers`);
}

/**
 * Install onto the fleet. 200 and 207 are ordinary replies; a 502 whose body
 * carries `results` means no proxy accepted it, which the page shows per
 * instance like any other outcome. Any other error body is thrown.
 */
export async function installFromRegistry(id: string, name: string): Promise<InstallResponse> {
  const resp = await request(`/api/registries/${encodeURIComponent(id)}/install`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name }),
  });
  const text = await resp.text();
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    body = undefined;
  }
  const parsed = body as Partial<InstallResponse> & { error?: string };
  if (resp.ok || (resp.status === 502 && Array.isArray(parsed?.results))) {
    return parsed as InstallResponse;
  }
  throw new ApiError(resp.status, (typeof parsed?.error === "string" ? parsed.error : text) || resp.statusText);
}

export function deleteSniffer(name: string): Promise<FanoutResponse> {
  return requestJson(`/api/fleet/sniffers/${encodeURIComponent(name)}`, {
    method: "DELETE",
  });
}

// --- config (proxied to wayhouse-controller) ---

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

// --- tunnel addresses (proxied to wayhouse-controller) ---

export function getTunnelAddresses(): Promise<TunnelAddresses> {
  return requestJson("/api/tunnel/addresses");
}

/** Frees an owner's address: the controller's registry `DELETE /peers|/proxy-peers/{name}`. */
export function releaseTunnelAddress(
  role: "origin" | "proxy",
  name: string,
): Promise<{ revision: number; released: string | null }> {
  const base = role === "origin" ? "origins" : "proxies";
  return requestJson(`/api/tunnel/${base}/${encodeURIComponent(name)}`, { method: "DELETE" });
}
