import { useEffect, useState } from "react";
import {
  ApiError,
  diffRevision,
  getCurrentConfig,
  listRevisions,
  rollbackTo,
  submitConfig,
} from "../api";
import type { RevisionSummary } from "../types";

export function ConfigView() {
  const [text, setText] = useState("");
  const [revision, setRevision] = useState<string | null>(null);
  const [revisions, setRevisions] = useState<RevisionSummary[]>([]);
  const [diff, setDiff] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  async function refresh() {
    try {
      const [current, revs] = await Promise.all([
        getCurrentConfig().catch((err) => {
          if (err instanceof ApiError && err.status === 404) {
            return { text: "", revision: null };
          }
          throw err;
        }),
        listRevisions().catch(() => [] as RevisionSummary[]),
      ]);
      setText(current.text);
      setRevision(current.revision);
      setRevisions(revs);
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

  async function submit() {
    try {
      const result = await submitConfig(text);
      setNotice(`accepted as revision ${result.revision}`);
      await refresh();
    } catch (err) {
      setNotice(err instanceof ApiError ? err.message : String(err));
    }
  }

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

  if (loading) return <p>loading…</p>;

  return (
    <div>
      {notice && <p className="notice">{notice}</p>}

      <section className="panel">
        <h3>current config {revision ? `(revision ${revision})` : "(none submitted yet)"}</h3>
        <textarea
          className="config-editor"
          value={text}
          onChange={(e) => setText(e.target.value)}
          spellCheck={false}
          rows={20}
        />
        <button onClick={submit}>submit as a new revision</button>
      </section>

      <section className="panel">
        <h3>revision history</h3>
        <table className="revisions-table">
          <thead>
            <tr>
              <th>revision</th>
              <th>size</th>
              <th>current</th>
              <th>actions</th>
            </tr>
          </thead>
          <tbody>
            {revisions.map((r) => (
              <tr key={r.revision}>
                <td>{r.revision}</td>
                <td>{r.size_bytes} B</td>
                <td>{r.current ? "current" : ""}</td>
                <td>
                  <button onClick={() => showDiff(r.revision)}>diff vs. current</button>
                  {!r.current && <button onClick={() => doRollback(r.revision)}>roll back to this</button>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {diff !== null && (
          <>
            <h4>diff</h4>
            <pre className="diff-view">{diff}</pre>
          </>
        )}
      </section>
    </div>
  );
}
