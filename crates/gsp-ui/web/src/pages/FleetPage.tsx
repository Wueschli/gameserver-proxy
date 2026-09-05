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
import type { FanoutResponse, FleetInstanceView } from "../types";
import {
    buildGroupTree,
    subtreeInstanceCount,
    subtreeUnhealthyCount,
    type GroupNode,
} from "../lib/groupTree";
import { Badge } from "../components/ui/Badge";
import { Button, Input } from "../components/ui/Button";

function summarizeFanout(result: FanoutResponse): string {
    const parts = result.results.map((r) =>
        r.error ? `${r.instance}: ${r.error}` : `${r.instance}: ${r.status}`,
    );
    return parts.join("; ") || "no instances known yet";
}

export function FleetPage() {
    const { instances, connected } = useFleetSocket();
    const [notice, setNotice] = useState<string | null>(null);

    function report(label: string, promise: Promise<unknown>) {
        promise
            .then((result) => {
                const text =
                    result &&
                    typeof result === "object" &&
                    "results" in (result as object)
                        ? summarizeFanout(result as FanoutResponse)
                        : "ok";
                setNotice(`${label}: ${text}`);
            })
            .catch((err) =>
                setNotice(
                    `${label} failed: ${err instanceof ApiError ? err.message : err}`,
                ),
            );
    }

    const tree = buildGroupTree(instances);

    return (
        <div className="max-w-4xl">
            <div className="mb-5 flex items-center justify-between">
                <h1 className="text-lg font-semibold text-ink">Fleet</h1>
                <span className="flex items-center gap-1.5 text-xs text-ink-muted">
                    <span
                        className={`h-1.5 w-1.5 rounded-full ${connected ? "bg-good" : "bg-bad"}`}
                    />
                    {connected ? "live" : "reconnecting…"}
                </span>
            </div>

            {notice && (
                <p className="mb-4 rounded border border-line bg-surface px-3 py-2 text-sm text-ink-muted">
                    {notice}
                </p>
            )}

            {instances.length === 0 ? (
                <p className="text-sm text-ink-muted">
                    No instances have pushed a fleet-state summary yet.
                </p>
            ) : (
                <div className="rounded border border-line bg-surface">
                    {tree.children.map((child) => (
                        <GroupRow
                            key={child.path}
                            node={child}
                            depth={0}
                            report={report}
                        />
                    ))}
                    {tree.instances.map((inst) => (
                        <InstanceBlock
                            key={inst.instance}
                            inst={inst}
                            depth={0}
                            report={report}
                        />
                    ))}
                </div>
            )}

            <BroadcastForms report={report} />
        </div>
    );
}

function GroupRow({
    node,
    depth,
    report,
}: {
    node: GroupNode;
    depth: number;
    report: (label: string, p: Promise<unknown>) => void;
}) {
    const [open, setOpen] = useState(depth < 1);
    const unhealthy = subtreeUnhealthyCount(node);
    const count = subtreeInstanceCount(node);

    return (
        <div className="border-b border-line last:border-b-0">
            <button
                onClick={() => setOpen((o) => !o)}
                className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-raised"
                style={{ paddingLeft: `${depth * 1.25 + 0.75}rem` }}
            >
                <span className="w-3 shrink-0 text-ink-faint">
                    {open ? "▾" : "▸"}
                </span>
                <span className="font-mono text-ink">{node.segment}/</span>
                <span className="text-ink-faint">
                    {count} instance{count === 1 ? "" : "s"}
                </span>
                {unhealthy > 0 && (
                    <Badge tone="bad">{unhealthy} unhealthy</Badge>
                )}
            </button>
            {open && (
                <div>
                    {node.children.map((child) => (
                        <GroupRow
                            key={child.path}
                            node={child}
                            depth={depth + 1}
                            report={report}
                        />
                    ))}
                    {node.instances.map((inst) => (
                        <InstanceBlock
                            key={inst.instance}
                            inst={inst}
                            depth={depth + 1}
                            report={report}
                        />
                    ))}
                </div>
            )}
        </div>
    );
}

function InstanceBlock({
    inst,
    depth,
    report,
}: {
    inst: FleetInstanceView;
    depth: number;
    report: (label: string, p: Promise<unknown>) => void;
}) {
    const [open, setOpen] = useState(true);
    const rows = inst.pools.flatMap((pool) =>
        pool.backends.map((b) => ({ pool, b })),
    );
    const unhealthy = rows.filter((r) => !r.b.healthy).length;

    return (
        <div className="border-b border-line last:border-b-0">
            <button
                onClick={() => setOpen((o) => !o)}
                className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-raised"
                style={{ paddingLeft: `${depth * 1.25 + 0.75}rem` }}
            >
                <span className="w-3 shrink-0 text-ink-faint">
                    {open ? "▾" : "▸"}
                </span>
                <span
                    className={`font-mono ${inst.stale ? "text-ink-faint" : "text-ink"}`}
                >
                    {inst.instance}
                </span>
                {inst.stale && <Badge tone="neutral">stale</Badge>}
                {unhealthy > 0 ? (
                    <Badge tone="bad">{unhealthy} unhealthy</Badge>
                ) : (
                    <Badge tone="good">healthy</Badge>
                )}
                <span className="ml-auto text-xs text-ink-faint">
                    {(inst.last_seen_ms_ago / 1000).toFixed(1)}s ago ·{" "}
                    {inst.sessions.tcp + inst.sessions.udp} sessions
                </span>
            </button>
            {open && (
                <div
                    className="pb-2"
                    style={{ paddingLeft: `${(depth + 1) * 1.25 + 0.75}rem` }}
                >
                    <div className="mb-2 flex gap-2">
                        <Button
                            variant="ghost"
                            onClick={() =>
                                report(
                                    `drain ${inst.instance}`,
                                    drainInstance(inst.instance),
                                )
                            }
                        >
                            Drain
                        </Button>
                        <Button
                            variant="ghost"
                            onClick={() =>
                                report(
                                    `undrain ${inst.instance}`,
                                    undrainInstance(inst.instance),
                                )
                            }
                        >
                            Undrain
                        </Button>
                    </div>
                    {rows.length === 0 ? (
                        <p className="text-xs text-ink-faint">
                            No pools reported.
                        </p>
                    ) : (
                        <>
                            <p className="mb-1.5 text-xs text-ink-faint">
                                Backend state and remove apply fleet-wide: every
                                instance with a matching pool/address applies
                                the change, others report 404 — see the result
                                notice above after acting.
                            </p>
                            <table className="w-full max-w-2xl text-xs">
                                <thead className="text-ink-faint">
                                    <tr className="text-left">
                                        <th className="py-1 font-medium">
                                            pool
                                        </th>
                                        <th className="py-1 font-medium">
                                            backend
                                        </th>
                                        <th className="py-1 font-medium">
                                            health
                                        </th>
                                        <th className="py-1 font-medium">
                                            state
                                        </th>
                                        <th className="py-1 font-medium">
                                            active
                                        </th>
                                        <th className="py-1"></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {rows.map(({ pool, b }) => (
                                        <tr
                                            key={`${pool.name}-${b.addr}`}
                                            className="border-t border-line/60"
                                        >
                                            <td className="py-1.5 text-ink-muted">
                                                {pool.name}
                                            </td>
                                            <td className="py-1.5 font-mono text-ink">
                                                {b.addr}
                                            </td>
                                            <td className="py-1.5">
                                                {b.healthy ? (
                                                    <Badge tone="good">
                                                        healthy
                                                    </Badge>
                                                ) : (
                                                    <Badge tone="bad">
                                                        unhealthy
                                                    </Badge>
                                                )}
                                            </td>
                                            <td className="py-1.5">
                                                <select
                                                    value={b.state}
                                                    onChange={(e) =>
                                                        report(
                                                            `patch ${pool.name}/${b.addr}`,
                                                            patchBackend(
                                                                pool.name,
                                                                b.addr,
                                                                e.target
                                                                    .value as
                                                                    | "enabled"
                                                                    | "draining"
                                                                    | "disabled",
                                                            ),
                                                        )
                                                    }
                                                    className="rounded border border-line bg-surface px-1.5 py-0.5 text-ink"
                                                >
                                                    <option value="enabled">
                                                        enabled
                                                    </option>
                                                    <option value="draining">
                                                        draining
                                                    </option>
                                                    <option value="disabled">
                                                        disabled
                                                    </option>
                                                </select>
                                            </td>
                                            <td className="py-1.5 text-ink-muted">
                                                {b.active}
                                            </td>
                                            <td className="py-1.5">
                                                <button
                                                    onClick={() =>
                                                        report(
                                                            `remove ${pool.name}/${b.addr}`,
                                                            deleteBackend(
                                                                pool.name,
                                                                b.addr,
                                                            ),
                                                        )
                                                    }
                                                    className="text-bad hover:underline"
                                                >
                                                    remove
                                                </button>
                                            </td>
                                        </tr>
                                    ))}
                                </tbody>
                            </table>
                        </>
                    )}
                </div>
            )}
        </div>
    );
}

function BroadcastForms({
    report,
}: {
    report: (label: string, p: Promise<unknown>) => void;
}) {
    const [addPool, setAddPool] = useState("");
    const [addAddr, setAddAddr] = useState("");
    const [hintIp, setHintIp] = useState("");
    const [hintPool, setHintPool] = useState("");
    const [hintTtl, setHintTtl] = useState(30);

    return (
        <div className="mt-6 grid grid-cols-2 gap-4">
            <section className="rounded border border-line bg-surface p-4">
                <h3 className="mb-3 text-sm font-medium text-ink">
                    Add a backend (every instance)
                </h3>
                <form
                    className="flex flex-wrap gap-2"
                    onSubmit={(e) => {
                        e.preventDefault();
                        report(
                            `add ${addPool}/${addAddr}`,
                            addBackend(addPool, addAddr),
                        );
                    }}
                >
                    <Input
                        placeholder="pool"
                        value={addPool}
                        onChange={(e) => setAddPool(e.target.value)}
                    />
                    <Input
                        placeholder="ip:port"
                        value={addAddr}
                        onChange={(e) => setAddAddr(e.target.value)}
                    />
                    <Button type="submit">Add</Button>
                </form>
            </section>

            <section className="rounded border border-line bg-surface p-4">
                <h3 className="mb-3 text-sm font-medium text-ink">
                    Route hint (every instance)
                </h3>
                <form
                    className="flex flex-wrap gap-2"
                    onSubmit={(e) => {
                        e.preventDefault();
                        report(
                            "route-hint",
                            routeHint(hintIp, hintPool, hintTtl),
                        );
                    }}
                >
                    <Input
                        placeholder="src ip"
                        value={hintIp}
                        onChange={(e) => setHintIp(e.target.value)}
                    />
                    <Input
                        placeholder="pool"
                        value={hintPool}
                        onChange={(e) => setHintPool(e.target.value)}
                    />
                    <Input
                        type="number"
                        placeholder="ttl sec"
                        value={hintTtl}
                        onChange={(e) => setHintTtl(Number(e.target.value))}
                        className="w-24"
                    />
                    <Button type="submit">Set hint</Button>
                </form>
            </section>
        </div>
    );
}
