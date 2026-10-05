import { useEffect, useRef, useState } from "react";
import type { FleetInstanceView } from "./types";

/**
 * Connects to wayhouse-ui's `/ws/fleet` (session-cookie gated, same as any other
 * fetch — the browser's cookie jar handles it automatically on the upgrade
 * request) and keeps the latest fleet view in state. Reconnects with a
 * fixed short delay on disconnect; wayhouse-ui's own `fleet_feed` already holds
 * the durable "last known view" server-side; this hook only needs to get
 * back to it, not replay anything.
 */
export function useFleetSocket(): {
  instances: FleetInstanceView[];
  connected: boolean;
} {
  const [instances, setInstances] = useState<FleetInstanceView[]>([]);
  const [connected, setConnected] = useState(false);
  const cancelled = useRef(false);

  useEffect(() => {
    cancelled.current = false;
    let socket: WebSocket | null = null;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;

    function connect() {
      if (cancelled.current) return;
      const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
      socket = new WebSocket(`${proto}//${window.location.host}/ws/fleet`);

      socket.onopen = () => setConnected(true);
      socket.onmessage = (event) => {
        try {
          const parsed = JSON.parse(event.data);
          if (Array.isArray(parsed)) {
            setInstances(parsed as FleetInstanceView[]);
          }
          // A non-array payload (e.g. "no aggregator configured") is a
          // one-shot notice, not fleet data — nothing to render it into.
        } catch {
          // Ignore an unparseable frame rather than crash the dashboard.
        }
      };
      socket.onclose = () => {
        setConnected(false);
        if (!cancelled.current) {
          retryTimer = setTimeout(connect, 2000);
        }
      };
      socket.onerror = () => socket?.close();
    }

    connect();

    return () => {
      cancelled.current = true;
      if (retryTimer) clearTimeout(retryTimer);
      socket?.close();
    };
  }, []);

  return { instances, connected };
}
