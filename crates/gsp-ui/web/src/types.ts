// Wire shapes from gsp-aggregator (relayed verbatim by gsp-ui's /ws/fleet
// and GET /api/fleet/*) and gsp-controller (proxied by GET/POST
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
