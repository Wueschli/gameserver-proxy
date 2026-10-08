import { useState } from "react";
import { ApiError, checkUpdates, listRegistrySniffers } from "../api";
import type { RegistrySniffer, RegistryRef, UpdateCheck, UpdateCheckRow } from "../types";
import { Badge } from "../components/ui/Badge";
import { Button } from "../components/ui/Button";
import { InstallDialog } from "./RegistrySection";

function message(err: unknown): string {
  return err instanceof ApiError || err instanceof Error ? err.message : String(err);
}

interface Pending {
  registry: RegistryRef;
  sniffer: RegistrySniffer;
  version: string;
}

/**
 * Checks for newer sniffer versions when the operator asks, never on a timer.
 * The update itself is the registry install flow for one exact version, with
 * the same limits and risk dialog.
 */
export function UpdatesSection({ onChanged }: { onChanged: (notice: string) => void }) {
  const [result, setResult] = useState<UpdateCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [opening, setOpening] = useState(false);

  async function check() {
    setChecking(true);
    setError(null);
    setResult(null);
    try {
      setResult(await checkUpdates());
    } catch (err) {
      setError(message(err));
    } finally {
      setChecking(false);
    }
  }

  async function startUpdate(row: UpdateCheckRow) {
    const update = row.update;
    if (!update?.compatible || opening) return;
    setOpening(true);
    setError(null);
    try {
      const listing = await listRegistrySniffers(update.registry_id);
      const sniffer = listing.sniffers.find((s) => s.name === row.sniffer);
      if (!sniffer) {
        setError(`${row.sniffer} is no longer listed in ${listing.registry.name}`);
        return;
      }
      setPending({ registry: listing.registry, sniffer, version: update.version });
    } catch (err) {
      setError(message(err));
    } finally {
      setOpening(false);
    }
  }

  return (
    <section className="mt-8" aria-labelledby="updates-heading">
      <div className="mb-3 flex items-center justify-between">
        <h2 id="updates-heading" className="text-base font-semibold text-ink">
          Updates
        </h2>
        <Button onClick={check} disabled={checking}>
          {checking ? "Checking…" : "Check for updates"}
        </Button>
      </div>
      {error && <p className="mb-3 text-sm text-bad">{error}</p>}
      {!result && !error && (
        <p className="text-sm text-ink-muted">
          Compares what the instances run with the registries, once, when you ask.
        </p>
      )}
      {result && <Results result={result} onUpdate={startUpdate} busy={opening} />}
      {pending && (
        <InstallDialog
          registry={pending.registry}
          sniffer={pending.sniffer}
          version={pending.version}
          verb="Update"
          onClose={() => setPending(null)}
          onInstalled={(name) => {
            // The list was true before the update; do not offer it again.
            setResult(null);
            onChanged(`updated ${name}`);
          }}
        />
      )}
    </section>
  );
}

function Results({
  result,
  onUpdate,
  busy,
}: {
  result: UpdateCheck;
  onUpdate: (row: UpdateCheckRow) => void;
  busy: boolean;
}) {
  const down = result.registries.filter((r) => !r.ok);
  return (
    <div className="flex flex-col gap-2">
      {down.map((r) => (
        <p key={r.id} className="rounded border border-warn/40 bg-warn/10 px-3 py-2 text-sm text-ink">
          Could not reach registry {r.name}: {r.error}
        </p>
      ))}
      {result.instance_errors.map((e) => (
        <p key={e.instance} className="rounded border border-line bg-surface px-3 py-2 text-sm text-ink-muted">
          {e.instance} did not answer (status {e.status}); it is missing from this list.
        </p>
      ))}
      <div className="rounded border border-line bg-surface">
        {result.sniffers.length === 0 ? (
          <p className="px-4 py-3 text-sm text-ink-muted">No sniffers are installed on the instances that answered.</p>
        ) : (
          <table className="w-full text-sm">
            <thead className="text-ink-faint">
              <tr className="text-left">
                <th className="px-4 py-2 font-medium">name</th>
                <th className="px-4 py-2 font-medium">installed</th>
                <th className="px-4 py-2 font-medium">instances</th>
                <th className="px-4 py-2 font-medium">update</th>
              </tr>
            </thead>
            <tbody>
              {result.sniffers.map((row) => (
                <tr key={`${row.sniffer}:${row.installed_sha256}`} className="border-t border-line align-top">
                  <td className="px-4 py-2 font-mono text-ink">{row.sniffer}</td>
                  <td className="px-4 py-2 text-ink-muted">
                    {row.known ? row.installed_version : <span title={row.installed_sha256}>unknown build</span>}
                  </td>
                  <td className="px-4 py-2 text-ink-muted">{row.instances.join(", ")}</td>
                  <td className="px-4 py-2">
                    <UpdateCell row={row} onUpdate={onUpdate} busy={busy} registryDown={down.length > 0} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      {!result.min_proxy_checked && (
        <p className="text-xs text-ink-faint">
          Compatibility is checked against sniffer ABI {result.host_abi}. Each sniffer&apos;s minimum proxy version is
          not checked yet.
        </p>
      )}
    </div>
  );
}

function UpdateCell({
  row,
  onUpdate,
  busy,
  registryDown,
}: {
  row: UpdateCheckRow;
  onUpdate: (row: UpdateCheckRow) => void;
  busy: boolean;
  registryDown: boolean;
}) {
  if (!row.known) {
    return <span className="text-ink-faint">not in any registry, so no update is offered</span>;
  }
  if (!row.update) {
    // An unreachable registry might hold a newer version, so "up to date" would be a guess.
    return registryDown ? (
      <span className="text-ink-muted">no update found in reachable registries</span>
    ) : (
      <Badge tone="good">up to date</Badge>
    );
  }
  if (!row.update.compatible) {
    return (
      <span className="text-warn">
        {row.update.version} is out: {row.update.reason}
      </span>
    );
  }
  return (
    <Button disabled={busy} onClick={() => onUpdate(row)}>
      Update to {row.update.version}
    </Button>
  );
}
