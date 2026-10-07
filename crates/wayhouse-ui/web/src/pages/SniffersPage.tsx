import { useEffect, useState } from "react";
import { useFleetSocket } from "../useFleetSocket";
import { ApiError, deleteSniffer, listInstanceSniffers, rollbackSniffer, uploadSniffer } from "../api";
import type { SnifferInfo } from "../types";
import { Badge } from "../components/ui/Badge";
import { Button, Input } from "../components/ui/Button";
import { Dialog } from "../components/ui/Dialog";
import { useConfirm } from "../components/ui/ConfirmDialog";
import { RegistrySection } from "./RegistrySection";
import { UpdatesSection } from "./UpdatesSection";

export function SniffersPage() {
  const { instances } = useFleetSocket();
  const [selected, setSelected] = useState<string | null>(null);
  const [modules, setModules] = useState<SnifferInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [uploadOpen, setUploadOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const { confirm, dialog } = useConfirm();

  const instanceName = selected ?? instances[0]?.instance ?? null;

  useEffect(() => {
    if (!instanceName) return;
    setError(null);
    listInstanceSniffers(instanceName)
      .then(setModules)
      .catch((err) => {
        setModules(null);
        setError(
          err instanceof ApiError && err.status === 409
            ? "this instance has no settings.sniffers configured"
            : err instanceof ApiError
              ? err.message
              : String(err),
        );
      });
  }, [instanceName, notice]);

  async function handleDelete(name: string) {
    const ok = await confirm({
      title: `Remove ${name}?`,
      description:
        "Deletes the module from every instance's sniffers directory; routes that use it stop matching after the next rescan.",
      confirmLabel: "Remove",
    });
    if (!ok) return;
    setBusy(true);
    try {
      await deleteSniffer(name);
      setNotice(`removed ${name}`);
    } catch (err) {
      setNotice(`remove ${name} failed: ${err instanceof ApiError ? err.message : err}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleRollback(name: string) {
    const ok = await confirm({
      title: `Roll back ${name}?`,
      description:
        "Every instance swaps this module with the version it kept before the last upload. Instances without a kept version are skipped. Pinned instances only roll back to a version their pin allows.",
      confirmLabel: "Roll back",
    });
    if (!ok) return;
    setBusy(true);
    try {
      const res = await rollbackSniffer(name);
      const done = res.results.filter((r) => r.status !== null && r.status >= 200 && r.status < 300).length;
      setNotice(`rolled back ${name} on ${done} of ${res.results.length} instances`);
    } catch (err) {
      setNotice(`roll back ${name} failed: ${err instanceof ApiError ? err.message : err}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="max-w-3xl">
      <div className="mb-5 flex items-center justify-between">
        <h1 className="text-lg font-semibold text-ink">Sniffers</h1>
        <Button variant="primary" onClick={() => setUploadOpen(true)}>
          Upload module
        </Button>
      </div>

      <div className="mb-4 flex items-center gap-2 text-sm">
        <label className="text-ink-muted">Instance</label>
        <select
          value={instanceName ?? ""}
          onChange={(e) => setSelected(e.target.value)}
          className="rounded border border-line bg-surface px-2 py-1 text-ink"
        >
          {instances.length === 0 && <option>no instances known yet</option>}
          {instances.map((i) => (
            <option key={i.instance} value={i.instance}>
              {i.instance}
            </option>
          ))}
        </select>
      </div>

      {notice && (
        <p className="mb-4 rounded border border-line bg-surface px-3 py-2 text-sm text-ink-muted">{notice}</p>
      )}

      <div className="rounded border border-line bg-surface">
        {error && <p className="px-4 py-3 text-sm text-ink-muted">{error}</p>}
        {!error && modules?.length === 0 && (
          <p className="px-4 py-3 text-sm text-ink-muted">No modules in this instance's sniffers.dir.</p>
        )}
        {!error && modules && modules.length > 0 && (
          <table className="w-full text-sm">
            <thead className="text-ink-faint">
              <tr className="text-left">
                <th className="px-4 py-2 font-medium">name</th>
                <th className="px-4 py-2 font-medium">sha256</th>
                <th className="px-4 py-2 font-medium">size</th>
                <th className="px-4 py-2 font-medium">status</th>
                <th className="px-4 py-2"></th>
              </tr>
            </thead>
            <tbody>
              {modules.map((m) => (
                <tr key={m.name} className="border-t border-line">
                  <td className="px-4 py-2 font-mono text-ink">{m.name}</td>
                  <td className="px-4 py-2 font-mono text-xs text-ink-faint">{m.sha256.slice(0, 12)}…</td>
                  <td className="px-4 py-2 text-ink-muted">{m.size_bytes} B</td>
                  <td className="px-4 py-2">
                    {m.loaded ? <Badge tone="good">loaded</Badge> : <Badge tone="warn">not loaded</Badge>}
                    {m.fallback && (
                      <span
                        className="ml-2"
                        title="The current file failed validation, so the previous version is running"
                      >
                        <Badge tone="warn">fallback active</Badge>
                      </span>
                    )}
                  </td>
                  <td className="px-4 py-2 text-right">
                    {m.has_previous && (
                      <button
                        onClick={() => handleRollback(m.name)}
                        disabled={busy}
                        className="mr-3 text-ink-muted hover:underline disabled:opacity-50"
                      >
                        roll back
                      </button>
                    )}
                    <button
                      onClick={() => handleDelete(m.name)}
                      disabled={busy}
                      className="text-bad hover:underline disabled:opacity-50"
                    >
                      remove
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <UpdatesSection onChanged={setNotice} />

      <RegistrySection onInstalled={(name) => setNotice(`installed ${name} from a registry`)} />

      {dialog}
      <UploadDialog
        open={uploadOpen}
        onOpenChange={setUploadOpen}
        onUploaded={(name) => setNotice(`uploaded ${name}`)}
      />
    </div>
  );
}

function UploadDialog({
  open,
  onOpenChange,
  onUploaded,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onUploaded: (name: string) => void;
}) {
  const [name, setName] = useState("");
  const [file, setFile] = useState<File | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!file) {
      setError("choose a .wasm file");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const bytes = await file.arrayBuffer();
      const moduleName = name || file.name.replace(/\.wasm$/, "");
      await uploadSniffer(moduleName, bytes);
      onUploaded(moduleName);
      onOpenChange(false);
      setName("");
      setFile(null);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title="Upload a sniffer module">
      <form className="flex flex-col gap-3" onSubmit={submit}>
        <label className="flex flex-col gap-1 text-sm text-ink-muted">
          Module name (defaults to the file name)
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. a2s" />
        </label>
        <label className="flex flex-col gap-1 text-sm text-ink-muted">
          .wasm file
          <input
            type="file"
            accept=".wasm"
            onChange={(e) => setFile(e.target.files?.[0] ?? null)}
            className="text-sm text-ink"
          />
        </label>
        {error && <p className="text-sm text-bad">{error}</p>}
        <div className="mt-2 flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={busy}>
            {busy ? "Uploading…" : "Upload to every instance"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
