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
