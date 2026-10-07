// Wire shapes from wayhouse-aggregator (relayed verbatim by wayhouse-ui's /ws/fleet
// and GET /api/fleet/*) and wayhouse-controller (proxied by GET/POST
// /api/config*). Kept in one file since both sides are simple, stable JSON
// this frontend only ever reads or round-trips, never transforms.

export interface BackendSummary {
  addr: string;
  healthy: boolean;
  state: string; // "enabled" | "draining" | "disabled"
  active: number;
}

export interface PoolSummary {
  name: string;
  balancer: string;
  backends: BackendSummary[];
}

export interface SessionCounts {
  tcp: number;
  udp: number;
}

export interface FleetInstanceView {
  instance: string;
  last_seen_ms_ago: number;
  stale: boolean;
  pools: PoolSummary[];
  sessions: SessionCounts;
  /** Self-reported fleet organization path, e.g. "eu/frankfurt/cluster-a". */
  group?: string | null;
}

export interface FanoutInstanceResult {
  instance: string;
  status: number | null;
  error: string | null;
}

export interface FanoutResponse {
  results: FanoutInstanceResult[];
}

export interface RevisionSummary {
  revision: number;
  size_bytes: number;
  current: boolean;
}

export interface SnifferInfo {
  name: string;
  sha256: string;
  size_bytes: number;
  loaded: boolean;
}

/** One row of wayhouse-controller's `GET /tunnel/addresses` (`docs/11` "Address authority"). */
export interface TunnelAddressEntry {
  role: "origin" | "proxy";
  name: string;
  address: string;
  /** Unix seconds. */
  first_seen: number;
  /** Unix seconds of the last registration. */
  last_seen: number;
  /** Not re-registered for longer than the controller's `--tunnel-stale-after`. */
  stale: boolean;
}

export interface TunnelAddresses {
  /** `null` when the controller runs without `--tunnel-network` (pinned addresses only). */
  network: string | null;
  allocated: number;
  capacity: number | null;
  entries: TunnelAddressEntry[];
}

/** One sniffer registry the UI knows (`GET /api/registries`). */
export interface RegistryRef {
  id: string;
  name: string;
  url: string;
  official: boolean;
  risk: "official" | "external";
}

export interface RegistryList {
  registries: RegistryRef[];
  /** False when the list lives in memory only and is lost on restart. */
  persistent: boolean;
}

export interface RegistryVersion {
  version: string;
  abi: string;
  min_proxy: string;
  url: string;
  sha256: string;
  size: number;
  signature_url?: string;
  limits: { max_memory_bytes: number; call_timeout_ms: number };
  config?: string;
}

/** `compatible` has `version` when this release can run it, else `reason`. */
export interface RegistrySniffer {
  name: string;
  description: string;
  license: string;
  homepage?: string;
  versions: RegistryVersion[];
  compatible: { version: string } | { reason: string };
}

export interface RegistrySniffers {
  registry: RegistryRef;
  index_name: string;
  host_abi: string;
  min_proxy_checked: boolean;
  sniffers: RegistrySniffer[];
}

export interface InstallInstanceResult {
  instance: string;
  ok: boolean;
  pinned: boolean;
  error: string | null;
  status: number | null;
  /** The start of a refusing proxy's reply. */
  detail?: string | null;
}

export interface InstallResponse {
  sniffer: string;
  version: string;
  signed: boolean;
  risk: "official" | "external";
  sha256: string;
  results: InstallInstanceResult[];
  pinned_instances: { instance: string; pin: { name: string; sha256: string } }[];
}
