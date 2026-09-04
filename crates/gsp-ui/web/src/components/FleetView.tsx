import { useState } from "react";
import { useFleetSocket } from "../useFleetSocket";
import {
  addBackend,
  deleteBackend,
  drainInstance,
  patchBackend,
  routeHint,
  undrainInstance,
  ApiError,
} from "../api";
import type { FanoutResponse } from "../types";

function summarizeFanout(result: FanoutResponse): string {
  const parts = result.results.map((r) =>
    r.error ? `${r.instance}: ${r.error}` : `${r.instance}: ${r.status}`,
  );
  return parts.join("; ") || "no instances known yet";
}

export function FleetView() {
  const { instances, connected } = useFleetSocket();
  const [notice, setNotice] = useState<string | null>(null);
  const [addPool, setAddPool] = useState("");
  const [addAddr, setAddAddr] = useState("");
  const [hintIp, setHintIp] = useState("");
  const [hintPool, setHintPool] = useState("");
  const [hintTtl, setHintTtl] = useState(30);

  function report(label: string, promise: Promise<unknown>) {
    promise
      .then((result) => {
        const text =
          result && typeof result === "object" && "results" in (result as object)
            ? summarizeFanout(result as FanoutResponse)
            : "ok";
        setNotice(`${label}: ${text}`);
      })
      .catch((err) => setNotice(`${label} failed: ${err instanceof ApiError ? err.message : err}`));
  }

  return (
    <div>
      <p className="ws-status">
        live feed: <span className={connected ? "ok" : "down"}>{connected ? "connected" : "reconnecting…"}</span>
      </p>
      {notice && <p className="notice">{notice}</p>}

      <table className="fleet-table">
        <thead>
          <tr>
            <th>instance</th>
            <th>last seen</th>
            <th>pool</th>
            <th>backend</th>
            <th>health</th>
            <th>state</th>
            <th>active</th>
            <th>actions</th>
          </tr>
        </thead>
        <tbody>
          {instances.length === 0 && (
            <tr>
              <td colSpan={8}>no instances have pushed yet</td>
            </tr>
          )}
          {instances.map((inst) => {
            const rows = inst.pools.flatMap((pool) =>
              pool.backends.map((b) => ({ pool, b })),
            );
            const rowSpan = Math.max(rows.length, 1);
            return rows.length === 0 ? (
              <tr key={inst.instance}>
                <InstanceCell inst={inst} rowSpan={1} onDrain={report} />
                <td colSpan={5}>no pools reported</td>
              </tr>
            ) : (
              rows.map(({ pool, b }, i) => (
                <tr key={`${inst.instance}-${pool.name}-${b.addr}`}>
                  {i === 0 && <InstanceCell inst={inst} rowSpan={rowSpan} onDrain={report} />}
                  <td>{pool.name}</td>
                  <td>{b.addr}</td>
                  <td className={b.healthy ? "ok" : "down"}>{b.healthy ? "healthy" : "unhealthy"}</td>
                  <td>
                    <select
                      value={b.state}
                      onChange={(e) =>
                        report(
                          `patch ${pool.name}/${b.addr}`,
                          patchBackend(pool.name, b.addr, e.target.value as "enabled" | "draining" | "disabled"),
                        )
                      }
                    >
                      <option value="enabled">enabled</option>
                      <option value="draining">draining</option>
                      <option value="disabled">disabled</option>
                    </select>
                  </td>
                  <td>{b.active}</td>
                  <td>
                    <button onClick={() => report(`remove ${pool.name}/${b.addr}`, deleteBackend(pool.name, b.addr))}>
                      remove
                    </button>
                  </td>
                </tr>
              ))
            );
          })}
        </tbody>
      </table>

      <section className="panel">
        <h3>add a backend (broadcast to every instance)</h3>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            report(`add ${addPool}/${addAddr}`, addBackend(addPool, addAddr));
          }}
        >
          <input placeholder="pool" value={addPool} onChange={(e) => setAddPool(e.target.value)} />
          <input placeholder="ip:port" value={addAddr} onChange={(e) => setAddAddr(e.target.value)} />
          <button type="submit">add</button>
        </form>
      </section>

      <section className="panel">
        <h3>route hint (broadcast to every instance)</h3>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            report("route-hint", routeHint(hintIp, hintPool, hintTtl));
          }}
        >
          <input placeholder="src ip" value={hintIp} onChange={(e) => setHintIp(e.target.value)} />
          <input placeholder="pool" value={hintPool} onChange={(e) => setHintPool(e.target.value)} />
          <input
            type="number"
            placeholder="ttl sec"
            value={hintTtl}
            onChange={(e) => setHintTtl(Number(e.target.value))}
          />
          <button type="submit">set hint</button>
        </form>
      </section>
    </div>
  );
}

function InstanceCell({
  inst,
  rowSpan,
  onDrain,
}: {
  inst: { instance: string; last_seen_ms_ago: number; stale: boolean };
  rowSpan: number;
  onDrain: (label: string, p: Promise<unknown>) => void;
}) {
  return (
    <>
      <td rowSpan={rowSpan} className={inst.stale ? "stale" : undefined}>
        {inst.instance}
        <div className="instance-actions">
          <button onClick={() => onDrain(`drain ${inst.instance}`, drainInstance(inst.instance))}>drain</button>
          <button onClick={() => onDrain(`undrain ${inst.instance}`, undrainInstance(inst.instance))}>
            undrain
          </button>
        </div>
      </td>
      <td rowSpan={rowSpan}>{(inst.last_seen_ms_ago / 1000).toFixed(1)}s ago</td>
    </>
  );
}
