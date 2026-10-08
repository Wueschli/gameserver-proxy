import { useEffect, useState } from "react";
import {
  ApiError,
  deletePlugin,
  installPlugin,
  listPlugins,
  setPluginEnabled,
  uploadPluginModule,
} from "../api";
import type { PluginCapabilities, PluginInstall, PluginModule } from "../types";
import { Badge } from "../components/ui/Badge";
import { Button, Input } from "../components/ui/Button";
import { Dialog } from "../components/ui/Dialog";
import { useConfirm } from "../components/ui/ConfirmDialog";

function ago(unixSecs: number): string {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - unixSecs);
  if (secs < 60) return "just now";
  if (secs < 3600) return `${Math.floor(secs / 60)} min ago`;
  if (secs < 86_400) return `${Math.floor(secs / 3600)} h ago`;
  return `${Math.floor(secs / 86_400)} d ago`;
}

function bytes(n: number): string {
  if (n >= 1024 * 1024) return `${+(n / (1024 * 1024)).toFixed(1)} MiB`;
  if (n >= 1024) return `${+(n / 1024).toFixed(1)} KiB`;
  return `${n} B`;
}

/** What a capability set lets a plugin do, in plain words. */
export function describeCapabilities(caps: PluginCapabilities): string[] {
  const out: string[] = [];
  if (caps.triggers.on_timer) out.push(`Runs every ${caps.tick_interval_secs} s`);
  if (caps.log) out.push("Writes log lines");
  if (caps.state) out.push(`Keeps up to ${bytes(caps.state.max_bytes)} of state`);
  if (out.length === 0) out.push("Asks for nothing");
  return out;
}

function CapabilityList({ caps }: { caps: PluginCapabilities }) {
  return (
    <ul className="list-disc pl-5 text-sm text-ink-muted">
      {describeCapabilities(caps).map((line) => (
        <li key={line}>{line}</li>
      ))}
    </ul>
  );
}

function errorText(err: unknown): string {
  return err instanceof ApiError ? err.message : String(err);
}

// Plugins on the controller (docs/plugins.md): upload, review what a module asks for,
// approve, then enable, disable or delete. Registry install is a later slice.
export function PluginsPage() {
  const [plugins, setPlugins] = useState<PluginInstall[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [uploadOpen, setUploadOpen] = useState(false);
  const { confirm, dialog } = useConfirm();

  async function refresh() {
    try {
      setPlugins(await listPlugins());
      setError(null);
    } catch (err) {
      setPlugins(null);
      setError(errorText(err));
    }
  }

  useEffect(() => {
    refresh();
  }, []);

  async function toggle(p: PluginInstall) {
    setBusy(true);
    try {
      await setPluginEnabled(p.id, !p.enabled);
      setNotice(`${p.enabled ? "disabled" : "enabled"} ${p.name}`);
    } catch (err) {
      setNotice(`${p.enabled ? "disable" : "enable"} ${p.name} failed: ${errorText(err)}`);
    } finally {
      setBusy(false);
    }
    await refresh();
  }

  async function remove(p: PluginInstall) {
    const ok = await confirm({
      title: `Delete ${p.name}?`,
      description:
        "Removes the install and its approval. The uploaded module is kept only while another install uses it.",
      confirmLabel: "Delete",
    });
    if (!ok) return;
    setBusy(true);
    try {
      await deletePlugin(p.id);
      setNotice(`deleted ${p.name}`);
    } catch (err) {
      setNotice(`delete ${p.name} failed: ${errorText(err)}`);
    } finally {
      setBusy(false);
    }
    await refresh();
  }

  return (
    <div className="max-w-4xl">
      <div className="mb-5 flex items-center justify-between">
        <h1 className="text-lg font-semibold text-ink">Plugins</h1>
        <Button variant="primary" onClick={() => setUploadOpen(true)}>
          Upload plugin
        </Button>
      </div>

      {notice && (
        <p className="mb-4 rounded border border-line bg-surface px-3 py-2 text-sm text-ink-muted">{notice}</p>
      )}

      <div className="rounded border border-line bg-surface">
        {error && <p className="px-4 py-3 text-sm text-ink-muted">{error}</p>}
        {!error && plugins === null && <p className="px-4 py-3 text-sm text-ink-muted">Loading…</p>}
        {!error && plugins?.length === 0 && (
          <p className="px-4 py-3 text-sm text-ink-muted">No plugins installed yet.</p>
        )}
        {!error && plugins && plugins.length > 0 && (
          <table className="w-full text-sm">
            <thead className="text-ink-faint">
              <tr className="text-left">
                <th className="px-4 py-2 font-medium">name</th>
                <th className="px-4 py-2 font-medium">approved</th>
                <th className="px-4 py-2 font-medium">module</th>
                <th className="px-4 py-2 font-medium">status</th>
                <th className="px-4 py-2" />
              </tr>
            </thead>
            <tbody>
              {plugins.map((p) => (
                <tr key={p.id} className="border-t border-line align-top">
                  <td className="px-4 py-2">
                    <div className="font-mono text-ink">{p.name}</div>
                    <div className="text-xs text-ink-faint">
                      installed {ago(p.created_at)}
                      {p.created_by ? ` by ${p.created_by}` : ""}
                    </div>
                  </td>
                  <td className="px-4 py-2">
                    <CapabilityList caps={p.approved} />
                  </td>
                  <td className="px-4 py-2">
                    <div className="font-mono text-xs text-ink-faint" title={p.sha256}>
                      {p.sha256.slice(0, 12)}…
                    </div>
                    <div className="text-xs text-ink-faint">{bytes(p.size)}</div>
                  </td>
                  <td className="px-4 py-2">
                    {p.enabled ? <Badge tone="good">enabled</Badge> : <Badge tone="neutral">disabled</Badge>}
                  </td>
                  <td className="px-4 py-2 text-right whitespace-nowrap">
                    <Button
                      className="mr-2"
                      disabled={busy}
                      aria-label={`${p.enabled ? "Disable" : "Enable"} ${p.name}`}
                      onClick={() => toggle(p)}
                    >
                      {p.enabled ? "Disable" : "Enable"}
                    </Button>
                    <Button
                      variant="danger"
                      disabled={busy}
                      aria-label={`Delete ${p.name}`}
                      onClick={() => remove(p)}
                    >
                      Delete
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <p className="mt-4 text-sm text-ink-faint">
        A plugin can still misuse what you approve: it can read and act on whatever its capabilities
        reach. Approve only modules you trust.
      </p>

      {dialog}
      <UploadDialog
        open={uploadOpen}
        onOpenChange={setUploadOpen}
        onInstalled={(name) => {
          setNotice(`installed ${name}`);
          refresh();
        }}
      />
    </div>
  );
}

/** A file name as a plugin name: lowercase a-z, 0-9 and '-', at most 64 characters. */
export function suggestName(fileName: string): string {
  return fileName
    .replace(/\.wasm$/i, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
}

function UploadDialog({
  open,
  onOpenChange,
  onInstalled,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onInstalled: (name: string) => void;
}) {
  const [file, setFile] = useState<File | null>(null);
  const [module, setModule] = useState<PluginModule | null>(null);
  const [name, setName] = useState("");
  const [enabled, setEnabled] = useState(true);
  const [approved, setApproved] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function reset() {
    setFile(null);
    setModule(null);
    setName("");
    setEnabled(true);
    setApproved(false);
    setError(null);
  }

  function close(next: boolean) {
    onOpenChange(next);
    if (!next) reset();
  }

  async function inspect(e: React.FormEvent) {
    e.preventDefault();
    if (!file) {
      setError("choose a .wasm file");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      setModule(await uploadPluginModule(await file.arrayBuffer()));
      setName(suggestName(file.name));
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!module || !approved) return;
    setBusy(true);
    setError(null);
    try {
      // Exactly the set the module declared: approval is bound to what was shown.
      await installPlugin({ name, sha256: module.sha256, approved: module.capabilities, enabled });
      onInstalled(name);
      close(false);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  if (!module) {
    return (
      <Dialog open={open} onOpenChange={close} title="Upload a plugin module">
        <form className="flex flex-col gap-3" onSubmit={inspect}>
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
            <Button type="button" variant="ghost" onClick={() => close(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="primary" disabled={busy}>
              {busy ? "Uploading…" : "Inspect module"}
            </Button>
          </div>
        </form>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={close} title="Review and approve">
      <form className="flex flex-col gap-3" onSubmit={submit}>
        <p className="text-sm text-ink-muted">
          Module <span className="font-mono text-xs text-ink">{module.sha256.slice(0, 12)}…</span>,{" "}
          {bytes(module.size)}, plugin ABI {module.abi}. Uploaded by hand: no registry vouches for it.
        </p>
        <div>
          <p className="mb-1 text-sm font-medium text-ink">This plugin asks to:</p>
          <CapabilityList caps={module.capabilities} />
          <p className="mt-2 text-sm text-ink-muted">
            It cannot make network requests, read secrets or change routes.
          </p>
        </div>
        <label className="flex flex-col gap-1 text-sm text-ink-muted">
          Name
          <Input value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <label className="flex items-center gap-2 text-sm text-ink-muted">
          <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
          Enable after installing
        </label>
        <label className="flex items-start gap-2 text-sm text-ink">
          <input
            type="checkbox"
            className="mt-1"
            checked={approved}
            onChange={(e) => setApproved(e.target.checked)}
          />
          I approve these capabilities for this exact module
        </label>
        {error && <p className="text-sm text-bad">{error}</p>}
        <div className="mt-2 flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => close(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={busy || !approved || name === ""}>
            Install
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
