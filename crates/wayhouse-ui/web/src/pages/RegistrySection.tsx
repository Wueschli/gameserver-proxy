import { useCallback, useEffect, useState } from "react";
import {
  ApiError,
  addRegistry,
  installFromRegistry,
  listRegistries,
  listRegistrySniffers,
  removeRegistry,
} from "../api";
import type {
  InstallInstanceResult,
  InstallResponse,
  RegistryRef,
  RegistrySniffer,
  RegistrySniffers,
  RegistryVersion,
} from "../types";
import { Badge } from "../components/ui/Badge";
import { Button, Input } from "../components/ui/Button";
import { Dialog } from "../components/ui/Dialog";
import { useConfirm } from "../components/ui/ConfirmDialog";
import { RegistryRiskDialog } from "../components/RegistryRiskDialog";

function message(err: unknown): string {
  return err instanceof ApiError || err instanceof Error ? err.message : String(err);
}

function formatBytes(n: number): string {
  const mib = n / (1024 * 1024);
  if (mib >= 1) return `${Number.isInteger(mib) ? mib : mib.toFixed(1)} MiB`;
  const kib = n / 1024;
  return kib >= 1 ? `${Number.isInteger(kib) ? kib : kib.toFixed(1)} KiB` : `${n} B`;
}

function RiskBadge({ registry }: { registry: RegistryRef }) {
  return registry.official ? (
    <Badge tone="good">official registry</Badge>
  ) : (
    <Badge tone="warn">external, at your own risk</Badge>
  );
}

/** The `settings.sniffers.modules` entry a pinned proxy needs for this module. */
function pinSnippet(pin: { name: string; sha256: string }): string {
  return `- name: ${pin.name}\n  sha256: ${pin.sha256}`;
}

/** Browse the configured registries and install a sniffer onto the whole fleet. */
export function RegistrySection({ onInstalled }: { onInstalled?: (name: string) => void }) {
  const [registries, setRegistries] = useState<RegistryRef[] | null>(null);
  const [persistent, setPersistent] = useState(true);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [listing, setListing] = useState<RegistrySniffers | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [installing, setInstalling] = useState<RegistrySniffer | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [removing, setRemoving] = useState(false);
  const { confirm, dialog } = useConfirm();

  const reload = useCallback(async (prefer?: string) => {
    try {
      const list = await listRegistries();
      setRegistries(list.registries);
      setPersistent(list.persistent);
      setSelectedId((cur) => {
        const want = prefer ?? cur;
        return list.registries.some((r) => r.id === want) ? want! : (list.registries[0]?.id ?? null);
      });
      setError(null);
    } catch (err) {
      setError(message(err));
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  useEffect(() => {
    if (!selectedId) {
      setListing(null);
      return;
    }
    let live = true;
    setListing(null);
    setError(null);
    listRegistrySniffers(selectedId)
      .then((l) => live && setListing(l))
      .catch((err) => live && setError(message(err)));
    return () => {
      live = false;
    };
  }, [selectedId]);

  const selected = registries?.find((r) => r.id === selectedId) ?? null;

  async function handleRemove() {
    if (!selected || removing) return;
    const ok = await confirm({
      title: `Remove registry ${selected.name}?`,
      description: "Only forgets the registry here. Sniffers already installed stay installed.",
      confirmLabel: "Remove",
    });
    if (!ok) return;
    setRemoving(true);
    try {
      await removeRegistry(selected.id);
      setNotice(`removed registry ${selected.name}`);
      await reload();
    } catch (err) {
      setNotice(`remove registry failed: ${message(err)}`);
    } finally {
      setRemoving(false);
    }
  }

  return (
    <section className="mt-8">
      <div className="mb-3 flex items-center justify-between">
        <h2 className="text-base font-semibold text-ink">Install from a registry</h2>
        <Button onClick={() => setAddOpen(true)}>Add registry</Button>
      </div>

      {!persistent && (
        <p className="mb-3 rounded border border-warn/40 bg-warn/10 px-3 py-2 text-sm text-ink">
          Registries added here are lost on restart: wayhouse-ui runs without <code>--registries-file</code>.
        </p>
      )}
      {notice && (
        <p className="mb-3 rounded border border-line bg-surface px-3 py-2 text-sm text-ink-muted">{notice}</p>
      )}

      <div className="mb-3 flex flex-wrap items-center gap-2 text-sm">
        <label htmlFor="registry-select" className="text-ink-muted">
          Registry
        </label>
        <select
          id="registry-select"
          value={selectedId ?? ""}
          onChange={(e) => setSelectedId(e.target.value)}
          className="rounded border border-line bg-surface px-2 py-1 text-ink"
        >
          {registries?.length === 0 && <option value="">no registries</option>}
          {registries?.map((r) => (
            <option key={r.id} value={r.id}>
              {r.official ? r.name : `${r.name} (external)`}
            </option>
          ))}
        </select>
        {selected && <RiskBadge registry={selected} />}
        {selected && (
          <button
            onClick={handleRemove}
            disabled={removing}
            className="ml-auto text-bad hover:underline disabled:opacity-50"
          >
            {removing ? "removing…" : "remove registry"}
          </button>
        )}
      </div>

      {selected && <p className="mb-3 break-all font-mono text-xs text-ink-faint">{selected.url}</p>}

      <div className="rounded border border-line bg-surface">
        {error && <p className="px-4 py-3 text-sm text-ink-muted">{error}</p>}
        {!error && registries?.length === 0 && (
          <p className="px-4 py-3 text-sm text-ink-muted">No registries yet. Add one by its index URL.</p>
        )}
        {!error && selected && !listing && <p className="px-4 py-3 text-sm text-ink-muted">Loading…</p>}
        {!error && listing && listing.sniffers.length === 0 && (
          <p className="px-4 py-3 text-sm text-ink-muted">This registry lists no sniffers.</p>
        )}
        {!error && listing && listing.sniffers.length > 0 && (
          <table className="w-full text-sm">
            <thead className="text-ink-faint">
              <tr className="text-left">
                <th className="px-4 py-2 font-medium">name</th>
                <th className="px-4 py-2 font-medium">description</th>
                <th className="px-4 py-2 font-medium">version</th>
                <th className="px-4 py-2"></th>
              </tr>
            </thead>
            <tbody>
              {listing.sniffers.map((s) => {
                const ok = "version" in s.compatible;
                return (
                  <tr key={s.name} className="border-t border-line align-top">
                    <td className="px-4 py-2 font-mono text-ink">{s.name}</td>
                    <td className="px-4 py-2 text-ink-muted">
                      {s.description}
                      <span className="ml-2 text-xs text-ink-faint">{s.license}</span>
                    </td>
                    <td className="px-4 py-2 text-ink-muted">
                      {"version" in s.compatible ? (
                        s.compatible.version
                      ) : (
                        <span className="text-warn">{s.compatible.reason}</span>
                      )}
                    </td>
                    <td className="px-4 py-2 text-right">
                      <Button disabled={!ok} onClick={() => setInstalling(s)}>
                        Install
                      </Button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
      {listing && !listing.min_proxy_checked && (
        <p className="mt-2 text-xs text-ink-faint">
          Compatibility is checked against sniffer ABI {listing.host_abi}. Each sniffer&apos;s minimum proxy version
          is not checked yet.
        </p>
      )}

      {dialog}
      <AddRegistryDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdded={async (r) => {
          setNotice(`added registry ${r.name}`);
          await reload(r.id);
        }}
      />
      {selected && installing && (
        <InstallDialog
          registry={selected}
          sniffer={installing}
          onClose={() => setInstalling(null)}
          onInstalled={onInstalled}
        />
      )}
    </section>
  );
}

function AddRegistryDialog({
  open,
  onOpenChange,
  onAdded,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onAdded: (r: RegistryRef) => void | Promise<void>;
}) {
  const [url, setUrl] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit() {
    setBusy(true);
    setError(null);
    try {
      const r = await addRegistry(url.trim(), name.trim() || undefined);
      setUrl("");
      setName("");
      onOpenChange(false);
      await onAdded(r);
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <RegistryRiskDialog
      open={open}
      onOpenChange={onOpenChange}
      title="Add a registry"
      confirmLabel="Add"
      onConfirm={submit}
      busy={busy || url.trim() === ""}
    >
      <label className="flex flex-col gap-1 text-ink-muted">
        Index URL
        <Input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://example.org/index.json" />
      </label>
      <label className="flex flex-col gap-1 text-ink-muted">
        Name (optional)
        <Input value={name} onChange={(e) => setName(e.target.value)} />
      </label>
      {error && <p className="text-bad">{error}</p>}
    </RegistryRiskDialog>
  );
}

function VersionDetails({ version }: { version: RegistryVersion }) {
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
      <dt className="text-ink-faint">Version</dt>
      <dd className="text-ink">{version.version}</dd>
      <dt className="text-ink-faint">Size</dt>
      <dd className="text-ink">{formatBytes(version.size)}</dd>
      <dt className="text-ink-faint">Memory limit</dt>
      <dd className="text-ink">{formatBytes(version.limits.max_memory_bytes)} per call</dd>
      <dt className="text-ink-faint">Time limit</dt>
      <dd className="text-ink">{version.limits.call_timeout_ms} ms per call</dd>
      <dt className="text-ink-faint">Signature</dt>
      <dd className="text-ink">{version.signature_url ? "listed in the registry" : "none listed"}</dd>
      {version.config && (
        <>
          <dt className="text-ink-faint">Config</dt>
          <dd className="text-ink-muted">{version.config}</dd>
        </>
      )}
    </dl>
  );
}

function InstallDialog({
  registry,
  sniffer,
  onClose,
  onInstalled,
}: {
  registry: RegistryRef;
  sniffer: RegistrySniffer;
  onClose: () => void;
  onInstalled?: (name: string) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<InstallResponse | null>(null);

  const wanted = "version" in sniffer.compatible ? sniffer.compatible.version : null;
  const version = sniffer.versions.find((v) => v.version === wanted);

  async function run() {
    setBusy(true);
    setError(null);
    try {
      const res = await installFromRegistry(registry.id, sniffer.name, wanted ?? undefined);
      setOutcome(res);
      if (res.results.some((r) => r.ok)) onInstalled?.(sniffer.name);
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  const title = `Install ${sniffer.name}`;
  if (outcome) {
    return (
      <Dialog open onOpenChange={(o) => !o && onClose()} title={title}>
        <InstallOutcome outcome={outcome} onClose={onClose} />
      </Dialog>
    );
  }

  const body = (
    <>
      <p className="text-sm text-ink-muted">{sniffer.description}</p>
      {version && <VersionDetails version={version} />}
      <p className="text-sm text-ink-muted">
        Installs on every instance, from <span className="font-mono">{registry.name}</span>.
      </p>
      {error && <p className="text-sm text-bad">{error}</p>}
    </>
  );

  if (!registry.official) {
    return (
      <RegistryRiskDialog
        open
        onOpenChange={(o) => !o && onClose()}
        title={title}
        confirmLabel="Install on every instance"
        onConfirm={run}
        busy={busy}
      >
        {body}
      </RegistryRiskDialog>
    );
  }
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={title}>
      <div className="flex flex-col gap-3">
        {body}
        <div className="mt-1 flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy} onClick={run}>
            Install on every instance
          </Button>
        </div>
      </div>
    </Dialog>
  );
}

function resultLabel(r: InstallInstanceResult): React.ReactNode {
  if (r.ok) return <Badge tone="good">installed</Badge>;
  if (r.pinned) return <Badge tone="warn">pinned</Badge>;
  return <Badge tone="bad">failed</Badge>;
}

function InstallOutcome({ outcome, onClose }: { outcome: InstallResponse; onClose: () => void }) {
  const ok = outcome.results.filter((r) => r.ok).length;
  const pins = new Map<string, { name: string; sha256: string }>();
  for (const p of outcome.pinned_instances) pins.set(`${p.pin.name}:${p.pin.sha256}`, p.pin);

  return (
    <div className="flex flex-col gap-3 text-sm">
      <p className="text-ink">
        {outcome.sniffer} {outcome.version} · {outcome.signed ? "signature verified" : "unsigned"}
      </p>
      <p className="text-ink-muted">
        Installed on {ok} of {outcome.results.length} instances.
      </p>
      <ul className="divide-y divide-line rounded border border-line">
        {outcome.results.map((r) => (
          <li key={r.instance} className="flex items-start justify-between gap-3 px-3 py-2">
            <span className="font-mono text-ink">{r.instance}</span>
            <span className="flex flex-col items-end gap-1 text-right">
              {resultLabel(r)}
              {!r.ok && !r.pinned && (
                <span className="text-xs text-ink-muted">
                  {r.error ?? r.detail ?? (r.status !== null ? `status ${r.status}` : "no answer")}
                </span>
              )}
            </span>
          </li>
        ))}
      </ul>
      {pins.size > 0 && (
        <div className="flex flex-col gap-2">
          <p className="text-ink-muted">
            Pinned instances only load listed modules. Add this to <code>settings.sniffers.modules</code> there, then
            install again:
          </p>
          {[...pins.values()].map((pin) => (
            <PinSnippet key={`${pin.name}:${pin.sha256}`} pin={pin} />
          ))}
        </div>
      )}
      <div className="flex justify-end">
        <Button variant="primary" onClick={onClose}>
          Close
        </Button>
      </div>
    </div>
  );
}

function PinSnippet({ pin }: { pin: { name: string; sha256: string } }) {
  const text = pinSnippet(pin);
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-start gap-2">
      <pre className="flex-1 overflow-x-auto rounded border border-line bg-surface-raised px-3 py-2 font-mono text-xs text-ink">
        {text}
      </pre>
      <Button
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(text);
            setCopied(true);
          } catch {
            setCopied(false);
          }
        }}
      >
        {copied ? "Copied" : "Copy"}
      </Button>
    </div>
  );
}
