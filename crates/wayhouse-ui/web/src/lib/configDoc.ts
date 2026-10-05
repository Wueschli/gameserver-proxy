import { parse, stringify } from "yaml";

/**
 * A pragmatic structured view over a subset of `wayhouse-config`'s schema
 * (`settings.*` scalars, and the common per-pool / per-listener fields) —
 * not the whole schema (routes/matchers/resolvers/backend_sources are large
 * and best edited as YAML directly today). Reading/writing through this
 * type never drops unrelated YAML content: `applyPatch` mutates a parsed
 * copy of the *whole* document and re-serializes it, so fields this UI has
 * no form for (routes, sniffer module pins, resolvers, ...) round-trip
 * untouched. This is the escape-hatch contract the raw YAML panel and the
 * structured form both rely on — see `SettingsPage`.
 */
export interface ConfigDoc {
  settings: {
    workers?: number;
    shutdown_grace_sec?: number;
    admin?: { listen?: string; auth_token?: string };
    limits?: {
      max_connections?: number;
      max_udp_sessions?: number;
      max_new_sessions_per_sec?: number;
    };
    geo_db?: string;
    group?: string;
    failure_domain?: string;
    gossip?: { bind?: string; seeds?: string[]; quorum_fraction?: number; psk?: string };
    sniffers?: { dir?: string };
  };
  pools: Array<{ name: string; balancer?: string; targets?: string[] }>;
  listeners: Array<{ name: string; bind: string; protocol?: string; pool?: string }>;
  // Anything else in the document (routes, resolvers, backend_sources,
  // sniffer module pins, ...) is preserved but not typed here.
  [key: string]: unknown;
}

export function parseConfigText(text: string): { doc: ConfigDoc | null; error: string | null } {
  if (text.trim() === "") {
    return { doc: { settings: {}, pools: [], listeners: [] }, error: null };
  }
  try {
    const parsed = parse(text) as Partial<ConfigDoc>;
    const doc: ConfigDoc = {
      settings: {},
      pools: [],
      listeners: [],
      ...parsed,
    };
    return { doc, error: null };
  } catch (err) {
    return { doc: null, error: err instanceof Error ? err.message : String(err) };
  }
}

export function stringifyConfigDoc(doc: ConfigDoc): string {
  return stringify(doc);
}
