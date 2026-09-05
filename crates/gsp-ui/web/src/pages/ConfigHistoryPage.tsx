import { useEffect, useState } from "react";
import { ApiError, diffRevision, listRevisions, rollbackTo } from "../api";
import type { RevisionSummary } from "../types";
import { Badge } from "../components/ui/Badge";

export function ConfigHistoryPage() {
  const [revisions, setRevisions] = useState<RevisionSummary[]>([]);
  const [diff, setDiff] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  async function refresh() {
    try {
      setRevisions(await listRevisions());
    } catch (err) {
      setNotice(err instanceof ApiError ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function showDiff(rev: number) {
    try {
      setDiff(await diffRevision(rev));
    } catch (err) {
      setNotice(err instanceof ApiError ? err.message : String(err));
    }
  }

  async function doRollback(rev: number) {
    try {
      const result = await rollbackTo(rev);
      setNotice(`rolled back to revision ${rev} as new revision ${result.revision}`);
      await refresh();
    } catch (err) {
      setNotice(err instanceof ApiError ? err.message : String(err));
    }
  }

  if (loading) return <p className="text-sm text-ink-muted">Loading…</p>;

  return (
    <div className="max-w-3xl">
      <h1 className="mb-5 text-lg font-semibold text-ink">Config history</h1>

      {notice && (
        <p className="mb-4 rounded border border-line bg-surface px-3 py-2 text-sm text-ink-muted">{notice}</p>
      )}

      <div className="rounded border border-line bg-surface">
        {revisions.length === 0 ? (
          <p className="px-4 py-3 text-sm text-ink-muted">No revisions submitted yet.</p>
        ) : (
          <table className="w-full text-sm">
            <thead className="text-ink-faint">
              <tr className="text-left">
                <th className="px-4 py-2 font-medium">revision</th>
                <th className="px-4 py-2 font-medium">size</th>
                <th className="px-4 py-2 font-medium"></th>
                <th className="px-4 py-2"></th>
              </tr>
            </thead>
            <tbody>
              {revisions.map((r) => (
                <tr key={r.revision} className="border-t border-line">
                  <td className="px-4 py-2 font-mono text-ink">{r.revision}</td>
                  <td className="px-4 py-2 text-ink-muted">{r.size_bytes} B</td>
                  <td className="px-4 py-2">{r.current && <Badge tone="good">current</Badge>}</td>
                  <td className="px-4 py-2 text-right">
                    <button onClick={() => showDiff(r.revision)} className="mr-3 text-accent hover:underline">
                      diff vs. current
                    </button>
                    {!r.current && (
                      <button onClick={() => doRollback(r.revision)} className="text-warn hover:underline">
                        roll back to this
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {diff !== null && (
        <section className="mt-4 rounded border border-line bg-surface p-4">
          <h3 className="mb-2 text-sm font-medium text-ink">Diff</h3>
          <pre className="overflow-x-auto rounded bg-bg p-3 font-mono text-xs text-ink-muted">{diff}</pre>
        </section>
      )}
    </div>
  );
}
